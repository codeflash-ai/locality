use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;

use locality_platform::capabilities::projection_cli_value;
use locality_platform::mount_cli_capabilities;
use locality_store::{
    ConnectionRecord, ConnectionRepository, ConnectorProfileRepository, MountConfig,
    MountRepository, ProjectionMode, SqliteStateStore, open_credential_store,
};
use serde::Serialize;
use serde_json::Value;

use crate::daemon::{DaemonControlReport, DaemonRunState, run_daemon_control};
use crate::file_provider;

#[derive(Clone, Debug, Default)]
pub struct DoctorOptions {
    pub state_root: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub command: &'static str,
    pub status: DoctorStatus,
    pub platform: String,
    pub state_root: String,
    pub daemon: Option<DoctorDaemon>,
    pub state_store: DoctorStateStore,
    pub connections: Vec<DoctorConnection>,
    pub mounts: Vec<DoctorMount>,
    pub findings: Vec<DoctorFinding>,
    pub suggested_commands: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Ok,
    Warning,
    Error,
}

impl DoctorStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorDaemon {
    pub state: String,
    pub manager: String,
    pub socket: String,
    pub tcp_addr: String,
    pub pid_file: Option<String>,
    pub stdout_log: Option<String>,
    pub stderr_log: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorStateStore {
    pub path: String,
    pub exists: bool,
    pub readable: bool,
    pub schema_version: Option<i64>,
    pub supported_schema_version: i64,
    pub compatibility_status: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorConnection {
    pub connection_id: String,
    pub connector: String,
    pub status: String,
    pub auth_kind: String,
    pub profile_id: Option<String>,
    pub profile_status: DoctorCheckState,
    pub credential_status: DoctorCheckState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorCheckState {
    Ok,
    Missing,
    Unavailable,
    NotChecked,
}

impl DoctorCheckState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Unavailable => "unavailable",
            Self::NotChecked => "not_checked",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorMount {
    pub mount_id: String,
    pub connector: String,
    pub root: String,
    pub root_exists: bool,
    pub projection: String,
    pub read_only: bool,
    pub connection_id: Option<String>,
    pub provider: Option<DoctorProvider>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorProvider {
    pub kind: String,
    pub state: String,
    pub registered: Option<bool>,
    pub helper: Option<String>,
    pub helper_present: bool,
    pub message: String,
    pub details: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct DoctorFinding {
    pub severity: DoctorSeverity,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_command: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorSeverity {
    Error,
    Warning,
    Info,
}

impl DoctorSeverity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

pub fn run_doctor(options: DoctorOptions) -> DoctorReport {
    let state_root = options
        .state_root
        .unwrap_or_else(locality_platform::default_state_root);
    let platform = std::env::consts::OS.to_string();
    let mut findings = Vec::new();
    let daemon = daemon_status(&state_root, &mut findings);
    let state_store = state_store_status(&state_root, &mut findings);
    let mut connections = Vec::new();
    let mut mounts = Vec::new();

    if state_store.exists && state_store.readable {
        match SqliteStateStore::open(state_root.clone()) {
            Ok(store) => {
                connections = connection_statuses(&state_root, &store, &mut findings);
                mounts = mount_statuses(&state_root, &store, daemon.as_ref(), &mut findings);
            }
            Err(error) => findings.push(DoctorFinding::global(
                DoctorSeverity::Error,
                "state_store_open_failed",
                format!("Could not open Locality state: {error}"),
                None,
            )),
        }
    }

    let status = report_status(&findings);
    let suggested_commands = suggested_commands(&findings);

    DoctorReport {
        ok: status != DoctorStatus::Error,
        command: "doctor",
        status,
        platform,
        state_root: state_root.display().to_string(),
        daemon,
        state_store,
        connections,
        mounts,
        findings,
        suggested_commands,
    }
}

pub fn doctor_exit_code(report: &DoctorReport) -> i32 {
    if report.status == DoctorStatus::Error {
        3
    } else {
        0
    }
}

pub fn print_doctor_report(report: &DoctorReport) {
    println!("Locality doctor: {}", report.status.as_str());
    println!("platform: {}", report.platform);
    println!("state: {}", report.state_root);
    println!(
        "state store: {} ({})",
        if report.state_store.exists {
            "present"
        } else {
            "missing"
        },
        report.state_store.compatibility_status
    );
    if let Some(daemon) = &report.daemon {
        println!("daemon: {} ({})", daemon.state, daemon.message);
    } else {
        println!("daemon: unavailable");
    }
    println!("connections: {}", report.connections.len());
    for connection in &report.connections {
        println!(
            "  {}: {} {} profile={} credential={}",
            connection.connection_id,
            connection.connector,
            connection.status,
            connection.profile_status.as_str(),
            connection.credential_status.as_str()
        );
    }
    println!("mounts: {}", report.mounts.len());
    for mount in &report.mounts {
        println!(
            "  {}: {} {} at {}",
            mount.mount_id, mount.connector, mount.projection, mount.root
        );
        if let Some(provider) = &mount.provider {
            let registered = provider
                .registered
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            println!(
                "    provider: {} state={} registered={} helper={}",
                provider.kind,
                provider.state,
                registered,
                if provider.helper_present {
                    "present"
                } else {
                    "missing"
                }
            );
        }
    }
    if report.findings.is_empty() {
        println!("findings: none");
    } else {
        println!("findings:");
        for finding in &report.findings {
            println!(
                "  [{}] {}: {}",
                finding.severity.as_str(),
                finding.code,
                finding.message
            );
            if let Some(command) = &finding.suggested_command {
                println!("        suggested: {command}");
            }
        }
    }
}

fn daemon_status(state_root: &Path, findings: &mut Vec<DoctorFinding>) -> Option<DoctorDaemon> {
    let args = vec![
        "status".to_string(),
        "--state-dir".to_string(),
        state_root.display().to_string(),
    ];
    match run_daemon_control(&args) {
        Ok(report) => Some(daemon_from_report(&report)),
        Err(error) => {
            findings.push(DoctorFinding::global(
                DoctorSeverity::Error,
                "daemon_status_failed",
                format!("Could not inspect daemon status: {}", error.message()),
                Some("loc daemon status".to_string()),
            ));
            None
        }
    }
}

fn daemon_from_report(report: &DaemonControlReport) -> DoctorDaemon {
    DoctorDaemon {
        state: report.state.as_str().to_string(),
        manager: format!("{:?}", report.manager),
        socket: report.socket.clone(),
        tcp_addr: report.tcp_addr.clone(),
        pid_file: report.pid_file.clone(),
        stdout_log: report.stdout_log.clone(),
        stderr_log: report.stderr_log.clone(),
        message: report.message.clone(),
    }
}

fn state_store_status(state_root: &Path, findings: &mut Vec<DoctorFinding>) -> DoctorStateStore {
    let compatibility = match SqliteStateStore::inspect_compatibility(state_root.to_path_buf()) {
        Ok(report) => report,
        Err(error) => {
            let path = state_root.join("state.sqlite3");
            findings.push(DoctorFinding::global(
                DoctorSeverity::Error,
                "state_store_unreadable",
                format!("Could not inspect Locality state store: {error}"),
                None,
            ));
            return DoctorStateStore {
                path: path.display().to_string(),
                exists: path.exists(),
                readable: false,
                schema_version: None,
                supported_schema_version: SqliteStateStore::current_schema_version(),
                compatibility_status: "error".to_string(),
            };
        }
    };

    let readable = matches!(
        compatibility.status,
        locality_store::StateCompatibilityStatus::Ready
            | locality_store::StateCompatibilityStatus::Migratable
    );
    let status = format!("{:?}", compatibility.status).to_lowercase();
    if !compatibility.db_exists {
        findings.push(DoctorFinding::global(
            DoctorSeverity::Info,
            "state_store_missing",
            "No Locality state store exists yet; connect and mount a workspace to initialize Locality.",
            Some("loc connect notion".to_string()),
        ));
    } else if !readable {
        findings.push(DoctorFinding::global(
            DoctorSeverity::Error,
            "state_store_incompatible",
            format!(
                "Locality state store is not readable by this binary: {:?}",
                compatibility.issues
            ),
            None,
        ));
    }

    DoctorStateStore {
        path: compatibility.db_path.display().to_string(),
        exists: compatibility.db_exists,
        readable,
        schema_version: compatibility.schema_version,
        supported_schema_version: compatibility.current_schema_version,
        compatibility_status: status,
    }
}

fn connection_statuses(
    state_root: &Path,
    store: &SqliteStateStore,
    findings: &mut Vec<DoctorFinding>,
) -> Vec<DoctorConnection> {
    let credentials = open_credential_store(state_root);
    let connections = match store.list_connections() {
        Ok(connections) => connections,
        Err(error) => {
            findings.push(DoctorFinding::global(
                DoctorSeverity::Error,
                "connections_unreadable",
                format!("Could not load connections: {error}"),
                None,
            ));
            return Vec::new();
        }
    };

    connections
        .into_iter()
        .map(|connection| {
            let profile_status = connection_profile_status(store, &connection, findings);
            let credential_status =
                connection_credential_status(credentials.as_ref(), &connection, findings);
            if connection.status != "active" {
                findings.push(DoctorFinding::connection(
                    DoctorSeverity::Error,
                    "connection_not_active",
                    format!(
                        "Connection `{}` is `{}`.",
                        connection.connection_id.0, connection.status
                    ),
                    &connection.connection_id.0,
                    Some("loc connect notion".to_string()),
                ));
            }
            DoctorConnection {
                connection_id: connection.connection_id.0,
                connector: connection.connector,
                status: connection.status,
                auth_kind: connection.auth_kind,
                profile_id: connection.profile_id.map(|profile| profile.0),
                profile_status,
                credential_status,
            }
        })
        .collect()
}

fn connection_profile_status(
    store: &SqliteStateStore,
    connection: &ConnectionRecord,
    findings: &mut Vec<DoctorFinding>,
) -> DoctorCheckState {
    let Some(profile_id) = &connection.profile_id else {
        findings.push(DoctorFinding::connection(
            DoctorSeverity::Warning,
            "connection_profile_missing",
            format!(
                "Connection `{}` does not reference a connector profile.",
                connection.connection_id.0
            ),
            &connection.connection_id.0,
            Some("loc connect notion".to_string()),
        ));
        return DoctorCheckState::Missing;
    };
    match store.get_connector_profile(profile_id) {
        Ok(Some(profile)) if profile.status == "active" => DoctorCheckState::Ok,
        Ok(Some(profile)) => {
            findings.push(DoctorFinding::connection(
                DoctorSeverity::Warning,
                "connection_profile_not_active",
                format!(
                    "Connector profile `{}` for connection `{}` is `{}`.",
                    profile.profile_id.0, connection.connection_id.0, profile.status
                ),
                &connection.connection_id.0,
                Some("loc connect notion".to_string()),
            ));
            DoctorCheckState::Unavailable
        }
        Ok(None) => {
            findings.push(DoctorFinding::connection(
                DoctorSeverity::Error,
                "connection_profile_missing",
                format!(
                    "Connector profile `{}` for connection `{}` is missing.",
                    profile_id.0, connection.connection_id.0
                ),
                &connection.connection_id.0,
                Some("loc connect notion".to_string()),
            ));
            DoctorCheckState::Missing
        }
        Err(error) => {
            findings.push(DoctorFinding::connection(
                DoctorSeverity::Error,
                "connection_profile_unreadable",
                format!(
                    "Could not inspect connector profile `{}` for connection `{}`: {error}",
                    profile_id.0, connection.connection_id.0
                ),
                &connection.connection_id.0,
                None,
            ));
            DoctorCheckState::Unavailable
        }
    }
}

fn connection_credential_status(
    credentials: &dyn locality_store::CredentialStore,
    connection: &ConnectionRecord,
    findings: &mut Vec<DoctorFinding>,
) -> DoctorCheckState {
    if connection.secret_ref.is_empty() {
        findings.push(DoctorFinding::connection(
            DoctorSeverity::Error,
            "connection_credential_missing",
            format!(
                "Connection `{}` does not reference a credential.",
                connection.connection_id.0
            ),
            &connection.connection_id.0,
            Some("loc connect notion".to_string()),
        ));
        return DoctorCheckState::Missing;
    }
    match credentials.get(&connection.secret_ref) {
        Ok(_) => DoctorCheckState::Ok,
        Err(locality_store::CredentialError::NotFound(_)) => {
            findings.push(DoctorFinding::connection(
                DoctorSeverity::Error,
                "connection_credential_missing",
                format!(
                    "Credential for connection `{}` is missing.",
                    connection.connection_id.0
                ),
                &connection.connection_id.0,
                Some("loc connect notion".to_string()),
            ));
            DoctorCheckState::Missing
        }
        Err(error) => {
            findings.push(DoctorFinding::connection(
                DoctorSeverity::Error,
                "connection_credential_unavailable",
                format!(
                    "Could not read credential for connection `{}`: {error}",
                    connection.connection_id.0
                ),
                &connection.connection_id.0,
                None,
            ));
            DoctorCheckState::Unavailable
        }
    }
}

fn mount_statuses(
    state_root: &Path,
    store: &SqliteStateStore,
    daemon: Option<&DoctorDaemon>,
    findings: &mut Vec<DoctorFinding>,
) -> Vec<DoctorMount> {
    let mounts = match store.load_mounts() {
        Ok(mounts) => mounts,
        Err(error) => {
            findings.push(DoctorFinding::global(
                DoctorSeverity::Error,
                "mounts_unreadable",
                format!("Could not load mounts: {error}"),
                None,
            ));
            return Vec::new();
        }
    };
    let supported = mount_cli_capabilities().supported_projections;

    mounts
        .into_iter()
        .map(|mount| {
            let root_exists = mount.root.exists();
            if !root_exists {
                findings.push(DoctorFinding::mount(
                    DoctorSeverity::Warning,
                    "mount_root_missing",
                    format!(
                        "Mount `{}` root does not exist: `{}`.",
                        mount.mount_id.0,
                        mount.root.display()
                    ),
                    &mount.mount_id.0,
                    None,
                ));
            }
            if !supported.contains(&mount.projection) {
                findings.push(DoctorFinding::mount(
                    DoctorSeverity::Error,
                    "projection_unsupported_on_platform",
                    format!(
                        "Mount `{}` uses `{}`, which is not supported on {}.",
                        mount.mount_id.0,
                        projection_cli_value(&mount.projection),
                        std::env::consts::OS
                    ),
                    &mount.mount_id.0,
                    None,
                ));
            }
            if mount.projection.uses_virtual_filesystem()
                && daemon.is_none_or(|daemon| daemon.state != DaemonRunState::Running.as_str())
            {
                findings.push(DoctorFinding::mount(
                    DoctorSeverity::Error,
                    "daemon_stopped_for_virtual_mount",
                    format!(
                        "Virtual mount `{}` needs localityd running.",
                        mount.mount_id.0
                    ),
                    &mount.mount_id.0,
                    Some("loc daemon start".to_string()),
                ));
            }
            let provider = provider_status(state_root, &mount, findings);
            DoctorMount {
                mount_id: mount.mount_id.0,
                connector: mount.connector,
                root: mount.root.display().to_string(),
                root_exists,
                projection: projection_cli_value(&mount.projection).to_string(),
                read_only: mount.read_only,
                connection_id: mount.connection_id.map(|connection_id| connection_id.0),
                provider,
            }
        })
        .collect()
}

fn provider_status(
    state_root: &Path,
    mount: &MountConfig,
    findings: &mut Vec<DoctorFinding>,
) -> Option<DoctorProvider> {
    match mount.projection {
        ProjectionMode::PlainFiles => None,
        ProjectionMode::MacosFileProvider => Some(macos_provider_status(mount, findings)),
        ProjectionMode::LinuxFuse => Some(linux_provider_status(mount, findings)),
        ProjectionMode::WindowsCloudFiles => {
            Some(windows_provider_status(state_root, mount, findings))
        }
    }
}

fn macos_provider_status(mount: &MountConfig, findings: &mut Vec<DoctorFinding>) -> DoctorProvider {
    #[cfg(target_os = "macos")]
    {
        let helper = file_provider::macos_file_provider_helper_path();
        let mut provider = DoctorProvider::from_helper_path(
            "macos_file_provider",
            helper.as_deref(),
            "unknown",
            "macOS File Provider status is unavailable.",
        );
        match file_provider::run_macos_file_provider_helper("list", Vec::new()) {
            Ok(report) => {
                let registered_domain_id = report
                    .helper_report
                    .get("domains")
                    .and_then(Value::as_array)
                    .and_then(|domains| {
                        domains.iter().find_map(|domain| {
                            let identifier = domain.get("identifier").and_then(Value::as_str)?;
                            if identifier == mount.mount_id.0
                                || identifier
                                    == localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID
                            {
                                Some(identifier.to_string())
                            } else {
                                None
                            }
                        })
                    });
                let registered = Some(registered_domain_id.is_some());
                provider.registered = registered;
                provider.state = if registered == Some(true) {
                    "registered".to_string()
                } else {
                    "unregistered".to_string()
                };
                provider.message = report
                    .helper_report
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("macOS File Provider status inspected.")
                    .to_string();
                provider.details = report.helper_report;
                if let Some(identifier) = registered_domain_id {
                    provider.details["registered_domain_id"] = Value::String(identifier);
                }
            }
            Err(error) => {
                provider.state = "error".to_string();
                provider.message = error.message();
            }
        }
        add_provider_findings(mount, &provider, findings, "loc file-provider register");
        provider
    }
    #[cfg(not(target_os = "macos"))]
    {
        let provider = DoctorProvider::unsupported(
            "macos_file_provider",
            "macOS File Provider can only run on macOS.",
        );
        add_provider_findings(mount, &provider, findings, "loc file-provider register");
        provider
    }
}

fn linux_provider_status(mount: &MountConfig, findings: &mut Vec<DoctorFinding>) -> DoctorProvider {
    #[cfg(target_os = "linux")]
    {
        let helper = file_provider::locality_fuse_helper_path();
        let service = file_provider::linux_fuse_unit_name(&mount.mount_id.0);
        let unit_path = file_provider::linux_fuse_unit_path(&service)
            .ok()
            .map(|path| path.display().to_string());
        let active = systemctl_user_output(&["is-active", &service]);
        let enabled = systemctl_user_output(&["is-enabled", &service]);
        let mut details = serde_json::json!({
            "service": service,
            "unit_path": unit_path,
            "active": active.as_deref(),
            "enabled": enabled.as_deref(),
        });
        let registered = unit_path
            .as_ref()
            .is_some_and(|path| Path::new(path).exists());
        details["registered"] = Value::Bool(registered);
        let state = if active.as_deref() == Some("active") {
            "running"
        } else if registered {
            "stopped"
        } else {
            "unregistered"
        };
        let provider = DoctorProvider {
            kind: "linux_fuse".to_string(),
            state: state.to_string(),
            registered: Some(registered),
            helper: helper.as_ref().map(|path| path.display().to_string()),
            helper_present: helper.is_some(),
            message: format!(
                "Linux FUSE systemd user service `{}` is `{}`.",
                service, state
            ),
            details,
        };
        add_provider_findings(mount, &provider, findings, "loc file-provider register");
        provider
    }
    #[cfg(not(target_os = "linux"))]
    {
        let provider =
            DoctorProvider::unsupported("linux_fuse", "Linux FUSE can only run on Linux.");
        add_provider_findings(mount, &provider, findings, "loc file-provider register");
        provider
    }
}

fn windows_provider_status(
    state_root: &Path,
    mount: &MountConfig,
    findings: &mut Vec<DoctorFinding>,
) -> DoctorProvider {
    let helper = file_provider::windows_cloud_files_helper_path();
    let mut provider = DoctorProvider::from_helper_path(
        "windows_cloud_files",
        helper.as_deref(),
        "unknown",
        "Windows Cloud Files status is unavailable.",
    );
    match file_provider::run_windows_cloud_files_lifecycle(
        state_root,
        mount,
        &mount.connector,
        file_provider::WindowsCloudFilesLifecycleAction::Status,
    ) {
        Ok(report) => {
            provider.state = report
                .helper_report
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            provider.registered = report
                .helper_report
                .get("registered")
                .and_then(Value::as_bool);
            provider.message = report
                .helper_report
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Windows Cloud Files status inspected.")
                .to_string();
            provider.details = report.helper_report;
        }
        Err(error) => {
            provider.state = "error".to_string();
            provider.message = error.message();
        }
    }
    add_provider_findings(mount, &provider, findings, "loc file-provider start");
    provider
}

impl DoctorProvider {
    fn from_helper_path(kind: &str, helper: Option<&Path>, state: &str, message: &str) -> Self {
        Self {
            kind: kind.to_string(),
            state: state.to_string(),
            registered: None,
            helper: helper.map(|path| path.display().to_string()),
            helper_present: helper.is_some(),
            message: message.to_string(),
            details: Value::Null,
        }
    }

    fn unsupported(kind: &str, message: &str) -> Self {
        Self {
            kind: kind.to_string(),
            state: "unsupported".to_string(),
            registered: None,
            helper: None,
            helper_present: false,
            message: message.to_string(),
            details: Value::Null,
        }
    }
}

fn add_provider_findings(
    mount: &MountConfig,
    provider: &DoctorProvider,
    findings: &mut Vec<DoctorFinding>,
    start_command: &str,
) {
    if !provider.helper_present && provider.state != "unsupported" {
        findings.push(DoctorFinding::mount(
            DoctorSeverity::Error,
            "provider_helper_missing",
            format!(
                "Provider helper for mount `{}` is missing.",
                mount.mount_id.0
            ),
            &mount.mount_id.0,
            None,
        ));
    }
    if provider.state == "unsupported" {
        findings.push(DoctorFinding::mount(
            DoctorSeverity::Error,
            "provider_unsupported_on_platform",
            format!("Mount `{}`: {}", mount.mount_id.0, provider.message),
            &mount.mount_id.0,
            None,
        ));
    } else if matches!(provider.registered, Some(false)) {
        findings.push(DoctorFinding::mount(
            DoctorSeverity::Error,
            "provider_unregistered",
            format!(
                "Provider for mount `{}` is not registered.",
                mount.mount_id.0
            ),
            &mount.mount_id.0,
            Some(format!("{start_command} {}", mount.mount_id.0)),
        ));
    } else if matches!(
        provider.state.as_str(),
        "stopped" | "unregistered" | "error"
    ) {
        findings.push(DoctorFinding::mount(
            DoctorSeverity::Error,
            "provider_not_running",
            format!(
                "Provider for mount `{}` is `{}`: {}",
                mount.mount_id.0, provider.state, provider.message
            ),
            &mount.mount_id.0,
            Some(format!("{start_command} {}", mount.mount_id.0)),
        ));
    }
    if provider
        .details
        .get("stale_pid_file")
        .and_then(Value::as_bool)
        == Some(true)
    {
        findings.push(DoctorFinding::mount(
            DoctorSeverity::Warning,
            "provider_stale_pid_file",
            format!(
                "Provider for mount `{}` has stale PID metadata.",
                mount.mount_id.0
            ),
            &mount.mount_id.0,
            Some(format!("{start_command} {}", mount.mount_id.0)),
        ));
    }
}

#[cfg(target_os = "linux")]
fn systemctl_user_output(args: &[&str]) -> Option<String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}

fn report_status(findings: &[DoctorFinding]) -> DoctorStatus {
    if findings
        .iter()
        .any(|finding| finding.severity == DoctorSeverity::Error)
    {
        DoctorStatus::Error
    } else if findings
        .iter()
        .any(|finding| finding.severity == DoctorSeverity::Warning)
    {
        DoctorStatus::Warning
    } else {
        DoctorStatus::Ok
    }
}

fn suggested_commands(findings: &[DoctorFinding]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut commands = Vec::new();
    for finding in findings {
        if let Some(command) = &finding.suggested_command
            && seen.insert(command.clone())
        {
            commands.push(command.clone());
        }
    }
    commands
}

impl DoctorFinding {
    fn global(
        severity: DoctorSeverity,
        code: impl Into<String>,
        message: impl Into<String>,
        suggested_command: Option<String>,
    ) -> Self {
        Self {
            severity,
            code: code.into(),
            message: message.into(),
            mount_id: None,
            connection_id: None,
            suggested_command,
        }
    }

    fn mount(
        severity: DoctorSeverity,
        code: impl Into<String>,
        message: impl Into<String>,
        mount_id: &str,
        suggested_command: Option<String>,
    ) -> Self {
        Self {
            severity,
            code: code.into(),
            message: message.into(),
            mount_id: Some(mount_id.to_string()),
            connection_id: None,
            suggested_command,
        }
    }

    fn connection(
        severity: DoctorSeverity,
        code: impl Into<String>,
        message: impl Into<String>,
        connection_id: &str,
        suggested_command: Option<String>,
    ) -> Self {
        Self {
            severity,
            code: code.into(),
            message: message.into(),
            mount_id: None,
            connection_id: Some(connection_id.to_string()),
            suggested_command,
        }
    }
}
