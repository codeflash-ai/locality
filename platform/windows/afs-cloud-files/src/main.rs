use std::fmt;
use std::path::{Path, PathBuf};

use afs_platform::{
    cloud_files_mount_id_component, decode_cloud_files_mount_id_component,
    windows_cloud_files_registration_marker_dir,
};
use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};

const COMMAND_NAME: &str = "afs-cloud-files";
const PROVIDER_ID: &str = "codeflash.ai.afs";
const SYNC_ROOT_ID_PREFIX: &str = "codeflash.ai.afs!default!";
#[cfg(target_os = "windows")]
const PROVIDER_GUID: u128 = 0xa4ee620b_cab8_4fc5_a942_68ad2854e19f;

#[cfg(target_os = "windows")]
fn trace_cloud_files(message: impl AsRef<str>) {
    if std::env::var_os("AFS_CLOUD_FILES_TRACE").is_some() {
        eprintln!("{COMMAND_NAME}: {}", message.as_ref());
    }
}

#[derive(Debug, Parser)]
#[command(name = COMMAND_NAME, about = "Manage AgentFS Windows Cloud Files sync roots.")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[arg(long, global = true)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    Register(RegisterArgs),
    Run(RunArgs),
    Open(OpenArgs),
    Unregister(UnregisterArgs),
    List(StateDirArgs),
    Reset(StateDirArgs),
}

#[derive(Debug, Args)]
struct RegisterArgs {
    #[arg(long)]
    mount_id: String,

    #[arg(long)]
    display_name: String,

    #[arg(long)]
    sync_root: PathBuf,

    #[arg(long)]
    state_dir: PathBuf,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(long)]
    mount_id: String,

    #[arg(long)]
    sync_root: PathBuf,

    #[arg(long)]
    state_dir: PathBuf,
}

#[derive(Debug, Args)]
struct OpenArgs {
    #[arg(long)]
    mount_id: String,

    #[arg(long)]
    sync_root: PathBuf,
}

#[derive(Debug, Args)]
struct UnregisterArgs {
    #[arg(long)]
    mount_id: String,

    #[arg(long)]
    state_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct StateDirArgs {
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
struct CommandReport {
    ok: bool,
    command: &'static str,
    action: &'static str,

    #[serde(skip_serializing_if = "Option::is_none")]
    mount_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sync_root_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    roots: Option<Vec<SyncRootReport>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cloud_filter_registered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shell_registered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shell_registration_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SyncRootReport {
    id: String,
    mount_id: Option<String>,
    display_name: Option<String>,
    path: Option<String>,
    version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ErrorReport {
    ok: bool,
    command: &'static str,
    action: &'static str,
    code: &'static str,
    message: String,
}

#[derive(Debug)]
struct HelperError {
    code: &'static str,
    message: String,
}

impl HelperError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn io(context: &str, error: std::io::Error) -> Self {
        Self::new("io_error", format!("{context}: {error}"))
    }
}

impl fmt::Display for HelperError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn main() {
    let cli = Cli::parse();
    let action = cli.command.action();
    match run(cli.command) {
        Ok(report) => {
            emit_success(&report, cli.json);
        }
        Err(error) => {
            emit_error(action, error, cli.json);
            std::process::exit(1);
        }
    }
}

fn run(command: Command) -> Result<CommandReport, HelperError> {
    match command {
        Command::Register(args) => register(args),
        Command::Run(args) => run_provider(args),
        Command::Open(args) => open(args),
        Command::Unregister(args) => unregister(args),
        Command::List(args) => list(args),
        Command::Reset(args) => reset(args),
    }
}

impl Command {
    fn action(&self) -> &'static str {
        match self {
            Self::Register(_) => "register",
            Self::Run(_) => "run",
            Self::Open(_) => "open",
            Self::Unregister(_) => "unregister",
            Self::List(_) => "list",
            Self::Reset(_) => "reset",
        }
    }
}

fn register(args: RegisterArgs) -> Result<CommandReport, HelperError> {
    ensure_supported_platform()?;
    validate_mount_id(&args.mount_id)?;
    validate_display_name(&args.display_name)?;
    validate_absolute_directory_candidate(&args.sync_root, "sync root")?;
    validate_absolute_directory_candidate(&args.state_dir, "state dir")?;

    let sync_root_id = sync_root_id_for_mount(&args.mount_id);
    let sync_root = prepare_directory(&args.sync_root, "create sync root")?;
    let state_dir = prepare_directory(&args.state_dir, "create state dir")?;

    register_cloud_filter_sync_root(&sync_root_id, &args.display_name, &sync_root)?;
    let shell_registration =
        register_shell_sync_root(&sync_root_id, &args.display_name, &sync_root);
    let (shell_registered, shell_registration_error) = match shell_registration {
        Ok(()) => (Some(true), None),
        Err(error) => (Some(false), Some(error.message)),
    };
    write_registration_marker(&state_dir, &args, &sync_root, &sync_root_id)?;

    Ok(CommandReport {
        ok: true,
        command: COMMAND_NAME,
        action: "register",
        mount_id: Some(args.mount_id),
        display_name: Some(args.display_name),
        sync_root: Some(path_for_report(&sync_root)),
        sync_root_id: Some(sync_root_id),
        provider_id: Some(PROVIDER_ID.to_string()),
        roots: None,
        cloud_filter_registered: Some(true),
        shell_registered,
        shell_registration_error,
    })
}

fn run_provider(args: RunArgs) -> Result<CommandReport, HelperError> {
    ensure_supported_platform()?;
    validate_mount_id(&args.mount_id)?;
    validate_absolute_directory_candidate(&args.sync_root, "sync root")?;
    validate_absolute_directory_candidate(&args.state_dir, "state dir")?;

    let sync_root_id = sync_root_id_for_mount(&args.mount_id);
    let sync_root = canonical_or_original(&args.sync_root);
    run_cloud_filter_provider(&args.mount_id, &sync_root, &args.state_dir)?;

    Ok(CommandReport {
        ok: true,
        command: COMMAND_NAME,
        action: "run",
        mount_id: Some(args.mount_id),
        display_name: None,
        sync_root: Some(path_for_report(&sync_root)),
        sync_root_id: Some(sync_root_id),
        provider_id: Some(PROVIDER_ID.to_string()),
        roots: None,
        cloud_filter_registered: None,
        shell_registered: None,
        shell_registration_error: None,
    })
}

fn open(args: OpenArgs) -> Result<CommandReport, HelperError> {
    ensure_supported_platform()?;
    validate_mount_id(&args.mount_id)?;
    validate_absolute_directory_candidate(&args.sync_root, "sync root")?;

    let sync_root = canonical_or_original(&args.sync_root);
    open_sync_root(&sync_root)?;

    Ok(CommandReport {
        ok: true,
        command: COMMAND_NAME,
        action: "open",
        mount_id: Some(args.mount_id.clone()),
        display_name: None,
        sync_root: Some(path_for_report(&sync_root)),
        sync_root_id: Some(sync_root_id_for_mount(&args.mount_id)),
        provider_id: Some(PROVIDER_ID.to_string()),
        roots: None,
        cloud_filter_registered: None,
        shell_registered: None,
        shell_registration_error: None,
    })
}

fn unregister(args: UnregisterArgs) -> Result<CommandReport, HelperError> {
    ensure_supported_platform()?;
    validate_mount_id(&args.mount_id)?;
    let sync_root_id = sync_root_id_for_mount(&args.mount_id);
    let marker = args.state_dir.as_deref().and_then(|state_dir| {
        read_registration_marker(state_dir, &args.mount_id)
            .ok()
            .flatten()
    });
    let shell_root = if marker.is_none() {
        list_shell_sync_roots()?
            .into_iter()
            .find(|root| root.id == sync_root_id)
    } else {
        None
    };
    let sync_root = marker
        .as_ref()
        .map(|marker| marker.sync_root.clone())
        .or_else(|| shell_root.as_ref().and_then(|root| root.path.clone()));
    if let Some(sync_root) = sync_root.as_deref() {
        unregister_cloud_filter_sync_root(Path::new(sync_root))?;
    }
    let _ = unregister_shell_sync_root(&sync_root_id);
    if let Some(state_dir) = args.state_dir.as_deref() {
        remove_registration_marker(state_dir, &args.mount_id)?;
    }

    Ok(CommandReport {
        ok: true,
        command: COMMAND_NAME,
        action: "unregister",
        mount_id: Some(args.mount_id),
        display_name: None,
        sync_root,
        sync_root_id: Some(sync_root_id),
        provider_id: Some(PROVIDER_ID.to_string()),
        roots: None,
        cloud_filter_registered: Some(false),
        shell_registered: Some(false),
        shell_registration_error: None,
    })
}

fn list(args: StateDirArgs) -> Result<CommandReport, HelperError> {
    ensure_supported_platform()?;
    let roots = match args.state_dir.as_deref() {
        Some(state_dir) => list_marker_sync_roots(state_dir)?,
        None => list_shell_sync_roots()?,
    };
    Ok(CommandReport {
        ok: true,
        command: COMMAND_NAME,
        action: "list",
        mount_id: None,
        display_name: None,
        sync_root: None,
        sync_root_id: None,
        provider_id: Some(PROVIDER_ID.to_string()),
        roots: Some(roots),
        cloud_filter_registered: None,
        shell_registered: None,
        shell_registration_error: None,
    })
}

fn reset(args: StateDirArgs) -> Result<CommandReport, HelperError> {
    ensure_supported_platform()?;
    let roots = match args.state_dir.as_deref() {
        Some(state_dir) => list_marker_sync_roots(state_dir)?,
        None => list_shell_sync_roots()?,
    };
    for root in &roots {
        if let Some(path) = root.path.as_deref() {
            unregister_cloud_filter_sync_root(Path::new(path))?;
        }
        let _ = unregister_shell_sync_root(&root.id);
        if let (Some(state_dir), Some(mount_id)) =
            (args.state_dir.as_deref(), root.mount_id.as_deref())
        {
            remove_registration_marker(state_dir, mount_id)?;
        }
    }

    Ok(CommandReport {
        ok: true,
        command: COMMAND_NAME,
        action: "reset",
        mount_id: None,
        display_name: None,
        sync_root: None,
        sync_root_id: None,
        provider_id: Some(PROVIDER_ID.to_string()),
        roots: Some(roots),
        cloud_filter_registered: Some(false),
        shell_registered: Some(false),
        shell_registration_error: None,
    })
}

fn emit_success(report: &CommandReport, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(report).expect("serialize cloud files report")
        );
        return;
    }

    match report.action {
        "list" => {
            let roots = report.roots.as_deref().unwrap_or(&[]);
            println!(
                "{} AgentFS Cloud Files sync root{}",
                roots.len(),
                plural(roots.len())
            );
            for root in roots {
                println!("  {} {}", root.id, root.path.as_deref().unwrap_or("-"));
            }
        }
        "reset" => {
            let roots = report.roots.as_deref().unwrap_or(&[]);
            println!(
                "unregistered {} AgentFS Cloud Files sync root{}",
                roots.len(),
                plural(roots.len())
            );
        }
        action => {
            println!(
                "{action} ok: {}",
                report
                    .sync_root_id
                    .as_deref()
                    .or(report.sync_root.as_deref())
                    .unwrap_or(PROVIDER_ID)
            );
        }
    }
}

fn emit_error(action: &'static str, error: HelperError, json: bool) {
    if json {
        let report = ErrorReport {
            ok: false,
            command: COMMAND_NAME,
            action,
            code: error.code,
            message: error.message,
        };
        println!(
            "{}",
            serde_json::to_string(&report).expect("serialize cloud files error")
        );
        return;
    }

    eprintln!("{} {action} failed: {}", COMMAND_NAME, error.message);
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn ensure_supported_platform() -> Result<(), HelperError> {
    #[cfg(target_os = "windows")]
    {
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(HelperError::new(
            "unsupported_platform",
            "Windows Cloud Files is only supported on Windows",
        ))
    }
}

fn validate_mount_id(mount_id: &str) -> Result<(), HelperError> {
    if mount_id.trim().is_empty() {
        return Err(HelperError::new(
            "invalid_args",
            "--mount-id cannot be empty",
        ));
    }
    Ok(())
}

fn validate_display_name(display_name: &str) -> Result<(), HelperError> {
    if display_name.trim().is_empty() {
        return Err(HelperError::new(
            "invalid_args",
            "--display-name cannot be empty",
        ));
    }
    Ok(())
}

fn validate_absolute_directory_candidate(path: &Path, label: &str) -> Result<(), HelperError> {
    if !path.is_absolute() {
        return Err(HelperError::new(
            "invalid_args",
            format!("{label} must be an absolute path: {}", path.display()),
        ));
    }
    Ok(())
}

fn prepare_directory(path: &Path, context: &str) -> Result<PathBuf, HelperError> {
    std::fs::create_dir_all(path).map_err(|error| HelperError::io(context, error))?;
    Ok(canonical_or_original(path))
}

fn canonical_or_original(path: &Path) -> PathBuf {
    platform_display_path(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

fn platform_display_path(path: PathBuf) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        strip_windows_verbatim_prefix(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        path
    }
}

#[cfg(target_os = "windows")]
fn strip_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path;
    };
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = value.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path
}

fn path_for_report(path: &Path) -> String {
    path.display().to_string()
}

fn sync_root_id_for_mount(mount_id: &str) -> String {
    format!(
        "{SYNC_ROOT_ID_PREFIX}{}",
        cloud_files_mount_id_component(mount_id)
    )
}

fn mount_id_from_sync_root_id(sync_root_id: &str) -> Option<String> {
    sync_root_id
        .strip_prefix(SYNC_ROOT_ID_PREFIX)
        .and_then(decode_cloud_files_mount_id_component)
}

fn registration_marker_dir(state_dir: &Path, mount_id: &str) -> PathBuf {
    windows_cloud_files_registration_marker_dir(state_dir, mount_id)
}

fn write_registration_marker(
    state_dir: &Path,
    args: &RegisterArgs,
    sync_root: &Path,
    sync_root_id: &str,
) -> Result<(), HelperError> {
    let marker_dir = registration_marker_dir(state_dir, &args.mount_id);
    std::fs::create_dir_all(&marker_dir)
        .map_err(|error| HelperError::io("create cloud files state", error))?;
    let marker = RegistrationMarker {
        mount_id: args.mount_id.clone(),
        display_name: args.display_name.clone(),
        sync_root: path_for_report(sync_root),
        sync_root_id: sync_root_id.to_string(),
        provider_id: PROVIDER_ID.to_string(),
    };
    let json = serde_json::to_string_pretty(&marker)
        .map_err(|error| HelperError::new("serialization_failed", error.to_string()))?;
    std::fs::write(marker_dir.join("registration.json"), json)
        .map_err(|error| HelperError::io("write cloud files registration marker", error))
}

fn read_registration_marker(
    state_dir: &Path,
    mount_id: &str,
) -> Result<Option<RegistrationMarker>, HelperError> {
    let marker_path = registration_marker_dir(state_dir, mount_id).join("registration.json");
    match std::fs::read_to_string(&marker_path) {
        Ok(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|error| HelperError::new("state_read_failed", error.to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(HelperError::io(
            "read cloud files registration marker",
            error,
        )),
    }
}

fn list_marker_sync_roots(state_dir: &Path) -> Result<Vec<SyncRootReport>, HelperError> {
    let root = state_dir.join("cloud-files");
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(HelperError::io("list cloud files registrations", error)),
    };

    let mut roots = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| HelperError::io("read cloud files registration", error))?;
        if !entry
            .file_type()
            .map_err(|error| HelperError::io("read cloud files registration type", error))?
            .is_dir()
        {
            continue;
        }
        let marker_path = entry.path().join("registration.json");
        let Ok(json) = std::fs::read_to_string(&marker_path) else {
            continue;
        };
        let marker = serde_json::from_str::<RegistrationMarker>(&json)
            .map_err(|error| HelperError::new("state_read_failed", error.to_string()))?;
        if marker.provider_id != PROVIDER_ID {
            continue;
        }
        roots.push(SyncRootReport {
            id: marker.sync_root_id,
            mount_id: Some(marker.mount_id),
            display_name: Some(marker.display_name),
            path: Some(marker.sync_root),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        });
    }
    Ok(roots)
}

fn remove_registration_marker(state_dir: &Path, mount_id: &str) -> Result<(), HelperError> {
    let marker_dir = registration_marker_dir(state_dir, mount_id);
    let marker_path = marker_dir.join("registration.json");
    match std::fs::remove_file(&marker_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(HelperError::io(
                "remove cloud files registration marker",
                error,
            ));
        }
    }
    match std::fs::remove_dir(&marker_dir) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(error) => {
            return Err(HelperError::io(
                "remove cloud files registration directory",
                error,
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistrationMarker {
    mount_id: String,
    display_name: String,
    sync_root: String,
    sync_root_id: String,
    provider_id: String,
}

#[cfg(target_os = "windows")]
fn register_cloud_filter_sync_root(
    sync_root_id: &str,
    display_name: &str,
    sync_root: &Path,
) -> Result<(), HelperError> {
    let _ = display_name;
    use windows::Win32::Storage::CloudFilters::{
        CF_HARDLINK_POLICY_NONE, CF_HYDRATION_POLICY, CF_HYDRATION_POLICY_FULL,
        CF_HYDRATION_POLICY_MODIFIER_ALLOW_FULL_RESTART_HYDRATION,
        CF_INSYNC_POLICY_TRACK_DIRECTORY_CREATION_TIME,
        CF_INSYNC_POLICY_TRACK_DIRECTORY_LAST_WRITE_TIME,
        CF_INSYNC_POLICY_TRACK_FILE_CREATION_TIME, CF_INSYNC_POLICY_TRACK_FILE_LAST_WRITE_TIME,
        CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT, CF_POPULATION_POLICY, CF_POPULATION_POLICY_FULL,
        CF_POPULATION_POLICY_MODIFIER_NONE, CF_REGISTER_FLAG_MARK_IN_SYNC_ON_ROOT,
        CF_REGISTER_FLAG_UPDATE, CF_SYNC_POLICIES, CF_SYNC_REGISTRATION, CfRegisterSyncRoot,
    };
    use windows::core::{GUID, PCWSTR};

    let sync_root_wide = wide_path(sync_root);
    let provider_name = wide_str("AgentFS");
    let provider_version = wide_str(env!("CARGO_PKG_VERSION"));
    let identity = sync_root_id.as_bytes();
    let root_identity = afsd::file_provider::ROOT_CONTAINER_IDENTIFIER.as_bytes();
    let registration = CF_SYNC_REGISTRATION {
        StructSize: std::mem::size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: PCWSTR::from_raw(provider_name.as_ptr()),
        ProviderVersion: PCWSTR::from_raw(provider_version.as_ptr()),
        SyncRootIdentity: identity.as_ptr().cast(),
        SyncRootIdentityLength: identity.len() as u32,
        FileIdentity: root_identity.as_ptr().cast(),
        FileIdentityLength: root_identity.len() as u32,
        ProviderId: GUID::from_u128(PROVIDER_GUID),
    };
    let policies = CF_SYNC_POLICIES {
        StructSize: std::mem::size_of::<CF_SYNC_POLICIES>() as u32,
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_FULL,
            Modifier: CF_HYDRATION_POLICY_MODIFIER_ALLOW_FULL_RESTART_HYDRATION,
        },
        Population: CF_POPULATION_POLICY {
            Primary: CF_POPULATION_POLICY_FULL,
            Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
        },
        InSync: CF_INSYNC_POLICY_TRACK_FILE_CREATION_TIME
            | CF_INSYNC_POLICY_TRACK_DIRECTORY_CREATION_TIME
            | CF_INSYNC_POLICY_TRACK_FILE_LAST_WRITE_TIME
            | CF_INSYNC_POLICY_TRACK_DIRECTORY_LAST_WRITE_TIME,
        HardLink: CF_HARDLINK_POLICY_NONE,
        PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
    };

    let register = |flags| unsafe {
        CfRegisterSyncRoot(
            PCWSTR::from_raw(sync_root_wide.as_ptr()),
            &registration,
            &policies,
            flags,
        )
    };
    register(CF_REGISTER_FLAG_MARK_IN_SYNC_ON_ROOT)
        .or_else(|_| register(CF_REGISTER_FLAG_UPDATE | CF_REGISTER_FLAG_MARK_IN_SYNC_ON_ROOT))
        .map_err(win32_error("register cloud filter sync root"))
}

#[cfg(not(target_os = "windows"))]
fn register_cloud_filter_sync_root(
    _sync_root_id: &str,
    _display_name: &str,
    _sync_root: &Path,
) -> Result<(), HelperError> {
    Err(HelperError::new(
        "unsupported_platform",
        "Windows Cloud Filter registration is only supported on Windows",
    ))
}

#[cfg(target_os = "windows")]
fn unregister_cloud_filter_sync_root(sync_root: &Path) -> Result<(), HelperError> {
    use windows::Win32::Storage::CloudFilters::CfUnregisterSyncRoot;
    use windows::core::PCWSTR;

    let sync_root_wide = wide_path(sync_root);
    unsafe { CfUnregisterSyncRoot(PCWSTR::from_raw(sync_root_wide.as_ptr())) }
        .map_err(win32_error("unregister cloud filter sync root"))
}

#[cfg(not(target_os = "windows"))]
fn unregister_cloud_filter_sync_root(_sync_root: &Path) -> Result<(), HelperError> {
    Err(HelperError::new(
        "unsupported_platform",
        "Windows Cloud Filter unregister is only supported on Windows",
    ))
}

#[cfg(target_os = "windows")]
const DAEMON_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(target_os = "windows")]
const DAEMON_READY_POLL: std::time::Duration = std::time::Duration::from_millis(250);
#[cfg(target_os = "windows")]
const DAEMON_PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(target_os = "windows")]
const METADATA_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
#[cfg(target_os = "windows")]
const MATERIALIZE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(target_os = "windows")]
const MUTATION_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(target_os = "windows")]
const LOCAL_CREATE_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
#[cfg(target_os = "windows")]
const LOCAL_CREATE_IO_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
#[cfg(target_os = "windows")]
const STATUS_SUCCESS_VALUE: i32 = 0;
#[cfg(target_os = "windows")]
const STATUS_UNSUCCESSFUL_VALUE: i32 = 0xC0000001_u32 as i32;

#[cfg(target_os = "windows")]
#[derive(Clone, Debug)]
struct ProviderContext {
    mount_id: String,
    sync_root: PathBuf,
    state_dir: PathBuf,
    identity_index: ProviderIdentityIndex,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Debug, Default)]
struct ProviderIdentityIndex {
    paths: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, String>>>,
}

#[cfg(target_os = "windows")]
impl ProviderIdentityIndex {
    fn remember(&self, path: &Path, identifier: &str) {
        if let Ok(mut paths) = self.paths.lock() {
            paths.insert(normalized_cloud_path_string(path), identifier.to_string());
        }
    }

    fn get(&self, path: &Path) -> Option<String> {
        self.paths
            .lock()
            .ok()
            .and_then(|paths| paths.get(&normalized_cloud_path_string(path)).cloned())
    }

    fn forget_subtree(&self, path: &Path) {
        let path = normalized_cloud_path_string(path);
        let prefix = format!("{path}\\");
        if let Ok(mut paths) = self.paths.lock() {
            let keys = paths
                .keys()
                .filter(|key| *key == &path || key.starts_with(&prefix))
                .cloned()
                .collect::<Vec<_>>();
            for key in keys {
                paths.remove(&key);
            }
        }
    }

    fn move_subtree(&self, source: &Path, target: &Path) {
        let source = normalized_cloud_path_string(source);
        let target = normalized_cloud_path_string(target);
        if source == target {
            return;
        }
        let source_prefix = format!("{source}\\");
        let mut moved = Vec::new();
        if let Ok(mut paths) = self.paths.lock() {
            let keys = paths.keys().cloned().collect::<Vec<_>>();
            for key in keys {
                if key == source {
                    if let Some(identifier) = paths.remove(&key) {
                        moved.push((target.clone(), identifier));
                    }
                } else if let Some(rest) = key.strip_prefix(&source_prefix)
                    && let Some(identifier) = paths.remove(&key)
                {
                    moved.push((format!("{target}\\{rest}"), identifier));
                }
            }
            for (path, identifier) in moved {
                paths.insert(path, identifier);
            }
        }
    }
}

#[cfg(target_os = "windows")]
impl ProviderContext {
    fn children(
        &self,
        container_identifier: &str,
    ) -> Result<afsd::file_provider::FileProviderChildrenReport, HelperError> {
        self.request(
            &afsd::ipc::DaemonRequest::FileProviderChildren {
                mount_id: self.mount_id.clone(),
                container_identifier: container_identifier.to_string(),
            },
            METADATA_REQUEST_TIMEOUT,
        )
    }

    fn read(
        &self,
        identifier: &str,
    ) -> Result<afsd::file_provider::FileProviderReadReport, HelperError> {
        self.request(
            &afsd::ipc::DaemonRequest::FileProviderRead {
                mount_id: self.mount_id.clone(),
                identifier: identifier.to_string(),
            },
            MATERIALIZE_REQUEST_TIMEOUT,
        )
    }

    fn commit_write(
        &self,
        identifier: &str,
        contents: &[u8],
    ) -> Result<afsd::virtual_fs::VirtualFsWriteReport, HelperError> {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD as BASE64;

        self.request(
            &afsd::ipc::DaemonRequest::VirtualFsCommitWrite {
                mount_id: self.mount_id.clone(),
                identifier: identifier.to_string(),
                contents_base64: BASE64.encode(contents),
            },
            MUTATION_REQUEST_TIMEOUT,
        )
    }

    fn create_file(
        &self,
        parent_identifier: &str,
        filename: &str,
    ) -> Result<afsd::virtual_fs::VirtualFsMutationReport, HelperError> {
        self.request(
            &afsd::ipc::DaemonRequest::VirtualFsCreateFile {
                mount_id: self.mount_id.clone(),
                parent_identifier: parent_identifier.to_string(),
                filename: filename.to_string(),
            },
            MUTATION_REQUEST_TIMEOUT,
        )
    }

    fn create_directory(
        &self,
        parent_identifier: &str,
        dirname: &str,
    ) -> Result<afsd::virtual_fs::VirtualFsMutationReport, HelperError> {
        self.request(
            &afsd::ipc::DaemonRequest::VirtualFsCreateDirectory {
                mount_id: self.mount_id.clone(),
                parent_identifier: parent_identifier.to_string(),
                dirname: dirname.to_string(),
            },
            MUTATION_REQUEST_TIMEOUT,
        )
    }

    fn rename(
        &self,
        identifier: &str,
        new_parent_identifier: &str,
        new_filename: &str,
    ) -> Result<afsd::virtual_fs::VirtualFsMutationReport, HelperError> {
        self.request(
            &afsd::ipc::DaemonRequest::VirtualFsRename {
                mount_id: self.mount_id.clone(),
                identifier: identifier.to_string(),
                new_parent_identifier: new_parent_identifier.to_string(),
                new_filename: new_filename.to_string(),
            },
            MUTATION_REQUEST_TIMEOUT,
        )
    }

    fn trash(
        &self,
        identifier: &str,
    ) -> Result<afsd::virtual_fs::VirtualFsMutationReport, HelperError> {
        self.request(
            &afsd::ipc::DaemonRequest::VirtualFsTrash {
                mount_id: self.mount_id.clone(),
                identifier: identifier.to_string(),
            },
            MUTATION_REQUEST_TIMEOUT,
        )
    }

    fn remember_path_identity(&self, path: &Path, identifier: &str) {
        self.identity_index
            .remember(&absolute_cloud_path(self, path), identifier);
    }

    fn cached_path_identity(&self, path: &Path) -> Option<String> {
        self.identity_index.get(&absolute_cloud_path(self, path))
    }

    fn forget_path_identities(&self, path: &Path) {
        self.identity_index
            .forget_subtree(&absolute_cloud_path(self, path));
    }

    fn move_path_identities(&self, source: &Path, target: &Path) {
        self.identity_index.move_subtree(
            &absolute_cloud_path(self, source),
            &absolute_cloud_path(self, target),
        );
    }

    fn request<T>(
        &self,
        request: &afsd::ipc::DaemonRequest,
        timeout: std::time::Duration,
    ) -> Result<T, HelperError>
    where
        T: serde::de::DeserializeOwned,
    {
        let response = afsd::ipc::send_request_with_timeout(&self.state_dir, request, timeout)
            .map_err(|error| HelperError::new("daemon_unavailable", error.message().to_string()))?;
        decode_daemon_response(response)
    }
}

#[cfg(target_os = "windows")]
struct ConnectedCloudProvider {
    connection_key: windows::Win32::Storage::CloudFilters::CF_CONNECTION_KEY,
    context: Box<ProviderContext>,
    local_change_watcher: Option<LocalChangeWatcher>,
}

#[cfg(target_os = "windows")]
impl Drop for ConnectedCloudProvider {
    fn drop(&mut self) {
        unsafe {
            let _ =
                windows::Win32::Storage::CloudFilters::CfDisconnectSyncRoot(self.connection_key);
        }
    }
}

#[cfg(target_os = "windows")]
struct LocalChangeWatcher {
    _watcher: notify::RecommendedWatcher,
}

#[cfg(target_os = "windows")]
fn start_local_change_watcher(context: ProviderContext) -> Result<LocalChangeWatcher, HelperError> {
    use notify::Watcher;

    let (sender, receiver) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |result| {
        let _ = sender.send(result);
    })
    .map_err(|error| HelperError::new("watcher_failed", error.to_string()))?;
    watcher
        .watch(&context.sync_root, notify::RecursiveMode::Recursive)
        .map_err(|error| HelperError::new("watcher_failed", error.to_string()))?;
    std::thread::Builder::new()
        .name("afs-cloud-files-local-changes".to_string())
        .spawn(move || local_change_worker(context, receiver))
        .map_err(|error| HelperError::new("watcher_failed", error.to_string()))?;
    Ok(LocalChangeWatcher { _watcher: watcher })
}

#[cfg(target_os = "windows")]
fn local_change_worker(
    context: ProviderContext,
    receiver: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
) {
    for result in receiver {
        match result {
            Ok(event) if is_create_like_event(&event.kind) => {
                trace_cloud_files(format!(
                    "local create-like event kind={:?} paths={:?}",
                    event.kind, event.paths
                ));
                std::thread::sleep(std::time::Duration::from_millis(250));
                for path in event.paths {
                    if let Err(error) = handle_local_create_like_path(&context, &path) {
                        eprintln!(
                            "{COMMAND_NAME}: local create mapping failed for `{}`: {error}",
                            path.display()
                        );
                    }
                }
            }
            Ok(event) if is_modify_like_event(&event.kind) => {
                trace_cloud_files(format!(
                    "local modify-like event kind={:?} paths={:?}",
                    event.kind, event.paths
                ));
                std::thread::sleep(std::time::Duration::from_millis(250));
                for path in event.paths {
                    if let Err(error) = handle_local_modify_like_path(&context, &path) {
                        eprintln!(
                            "{COMMAND_NAME}: local modify mapping failed for `{}`: {error}",
                            path.display()
                        );
                    }
                }
            }
            Ok(event) if is_remove_like_event(&event.kind) => {
                trace_cloud_files(format!(
                    "local remove-like event kind={:?} paths={:?}",
                    event.kind, event.paths
                ));
                for path in event.paths {
                    if let Err(error) = handle_local_remove_like_path(&context, &path) {
                        eprintln!(
                            "{COMMAND_NAME}: local remove mapping failed for `{}`: {error}",
                            path.display()
                        );
                    }
                }
            }
            Ok(_) => {}
            Err(error) => eprintln!("{COMMAND_NAME}: local change watcher failed: {error}"),
        }
    }
}

#[cfg(target_os = "windows")]
fn is_create_like_event(kind: &notify::event::EventKind) -> bool {
    use notify::event::{CreateKind, EventKind, ModifyKind, RenameMode};

    matches!(
        kind,
        EventKind::Create(CreateKind::Any | CreateKind::File | CreateKind::Folder)
            | EventKind::Modify(ModifyKind::Name(
                RenameMode::Any | RenameMode::To | RenameMode::Both
            ))
    )
}

#[cfg(target_os = "windows")]
fn is_remove_like_event(kind: &notify::event::EventKind) -> bool {
    use notify::event::{EventKind, RemoveKind};

    matches!(
        kind,
        EventKind::Remove(RemoveKind::Any | RemoveKind::File | RemoveKind::Folder)
    )
}

#[cfg(target_os = "windows")]
fn is_modify_like_event(kind: &notify::event::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind};

    matches!(
        kind,
        EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Access(AccessKind::Close(AccessMode::Write))
    )
}

#[cfg(target_os = "windows")]
fn handle_local_create_like_path(
    context: &ProviderContext,
    path: &Path,
) -> Result<(), HelperError> {
    let path = absolute_cloud_path(context, path);
    if !path_is_under_sync_root(context, &path) || same_cloud_path(&path, &context.sync_root) {
        return Ok(());
    }
    if placeholder_identity_for_path(&path)?.is_some() {
        return Ok(());
    }

    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(HelperError::io("inspect local create", error)),
    };
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| HelperError::new("invalid_path", "created path has no UTF-8 filename"))?;
    let parent = path
        .parent()
        .ok_or_else(|| HelperError::new("invalid_path", "created path has no parent"))?;
    let parent_identifier = parent_identifier_for_path_when_ready(context, parent)?;

    if metadata.is_dir() {
        let created = context.create_directory(&parent_identifier, filename)?;
        context.remember_path_identity(&path, &created.identifier);
        convert_to_placeholder_when_ready(&path, &created.identifier, false)?;
        let _ = set_placeholder_in_sync_state(&path, false);
        return Ok(());
    }

    if metadata.is_file() {
        let created = context.create_file(&parent_identifier, filename)?;
        context.remember_path_identity(&path, &created.identifier);
        let contents = read_created_file_when_ready(&path)?;
        if !contents.is_empty() {
            commit_local_bytes(context, &created.identifier, &path, &contents)?;
        }
        convert_to_placeholder_when_ready(&path, &created.identifier, false)?;
        let _ = set_placeholder_in_sync_state(&path, false);
        if let Some(parent_identifier) = context.cached_path_identity(parent)
            && placeholder_identity_for_path(parent)?.is_none()
            && convert_to_placeholder_when_ready(parent, &parent_identifier, false).is_ok()
        {
            let _ = set_placeholder_in_sync_state(parent, false);
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn handle_local_modify_like_path(
    context: &ProviderContext,
    path: &Path,
) -> Result<(), HelperError> {
    let path = absolute_cloud_path(context, path);
    if !path_is_under_sync_root(context, &path) || same_cloud_path(&path, &context.sync_root) {
        return Ok(());
    }
    let placeholder = placeholder_info_for_path(&path)?;
    let identifier = if let Some(info) = placeholder {
        context.remember_path_identity(&path, &info.identity);
        if info.in_sync {
            trace_cloud_files(format!(
                "local modify skipped path=`{}` reason=placeholder_in_sync",
                path.display()
            ));
            return Ok(());
        }
        Some(info.identity)
    } else {
        identity_for_path(context, &path)?
    };
    let Some(identifier) = identifier else {
        trace_cloud_files(format!(
            "local modify skipped path=`{}` reason=no_placeholder_identity",
            path.display()
        ));
        return Ok(());
    };
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(HelperError::io("inspect local modify", error)),
    };
    if !metadata.is_file() {
        return Ok(());
    }
    let contents = read_created_file_when_ready(&path)?;
    trace_cloud_files(format!(
        "local modify commit path=`{}` identity=`{identifier}` bytes={}",
        path.display(),
        contents.len()
    ));
    commit_local_bytes(context, &identifier, &path, &contents)
}

#[cfg(target_os = "windows")]
fn handle_local_remove_like_path(
    context: &ProviderContext,
    path: &Path,
) -> Result<(), HelperError> {
    let path = absolute_cloud_path(context, path);
    if !path_is_under_sync_root(context, &path) || same_cloud_path(&path, &context.sync_root) {
        return Ok(());
    }
    let Some(identifier) = identity_for_path(context, &path)? else {
        trace_cloud_files(format!(
            "local remove skipped path=`{}` reason=no_identity",
            path.display()
        ));
        return Ok(());
    };
    trace_cloud_files(format!(
        "local remove trash path=`{}` identity=`{identifier}`",
        path.display()
    ));
    match context.trash(&identifier) {
        Ok(_) => {
            context.forget_path_identities(&path);
            Ok(())
        }
        Err(error) if stale_pending_page_directory_delete(&identifier, &error) => {
            context.forget_path_identities(&path);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "windows")]
fn parent_identifier_for_path_when_ready(
    context: &ProviderContext,
    parent: &Path,
) -> Result<String, HelperError> {
    retry_local_create_operation(|| parent_identifier_for_path(context, parent))
}

#[cfg(target_os = "windows")]
fn identity_for_path(
    context: &ProviderContext,
    path: &Path,
) -> Result<Option<String>, HelperError> {
    let path = absolute_cloud_path(context, path);
    if let Some(identifier) = placeholder_identity_for_path(&path)? {
        context.remember_path_identity(&path, &identifier);
        return Ok(Some(identifier));
    }
    if let Some(identifier) = context.cached_path_identity(&path) {
        if is_local_identity(&identifier) {
            match daemon_identity_for_path(context, &path) {
                Ok(Some(refreshed)) => return Ok(Some(refreshed)),
                Ok(None) => {}
                Err(error) if error.code == "daemon_unavailable" => {}
                Err(error) => return Err(error),
            }
        }
        return Ok(Some(identifier));
    }
    daemon_identity_for_path(context, &path)
}

#[cfg(target_os = "windows")]
fn is_local_identity(identifier: &str) -> bool {
    identifier.starts_with("local:") || identifier.starts_with("children:local:")
}

#[cfg(target_os = "windows")]
fn daemon_identity_for_path(
    context: &ProviderContext,
    path: &Path,
) -> Result<Option<String>, HelperError> {
    let relative_path = match relative_cloud_path(context, path) {
        Some(relative_path) => relative_path,
        None => return Ok(None),
    };
    if relative_path.as_os_str().is_empty() {
        return Ok(Some(
            afsd::file_provider::ROOT_CONTAINER_IDENTIFIER.to_string(),
        ));
    }

    let mut current_identifier = afsd::file_provider::ROOT_CONTAINER_IDENTIFIER.to_string();
    let mut current_path = context.sync_root.clone();
    for component in relative_path.components() {
        let std::path::Component::Normal(component) = component else {
            return Ok(None);
        };
        let Some(component) = component.to_str() else {
            return Ok(None);
        };
        let children = context.children(&current_identifier)?;
        remember_placeholder_children(context, &current_path, &children.children);
        let Some(child) = children
            .children
            .iter()
            .find(|child| child.filename.eq_ignore_ascii_case(component))
        else {
            return Ok(None);
        };
        current_path.push(&child.filename);
        current_identifier = child.identifier.clone();
    }

    context.remember_path_identity(path, &current_identifier);
    Ok(Some(current_identifier))
}

#[cfg(target_os = "windows")]
fn relative_cloud_path(context: &ProviderContext, path: &Path) -> Option<PathBuf> {
    if let Ok(relative) = path.strip_prefix(&context.sync_root) {
        return Some(relative.to_path_buf());
    }

    let path = normalized_cloud_path_string(path);
    let root = normalized_cloud_path_string(&context.sync_root);
    if path == root {
        return Some(PathBuf::new());
    }
    path.strip_prefix(&(root + r"\")).map(PathBuf::from)
}

#[cfg(target_os = "windows")]
fn read_created_file_when_ready(path: &Path) -> Result<Vec<u8>, HelperError> {
    retry_local_create_operation(|| {
        std::fs::read(path).map_err(|error| HelperError::io("read created file", error))
    })
}

#[cfg(target_os = "windows")]
fn convert_to_placeholder_when_ready(
    path: &Path,
    identifier: &str,
    in_sync: bool,
) -> Result<(), HelperError> {
    retry_local_create_operation(|| convert_to_placeholder(path, identifier, in_sync))
}

#[cfg(target_os = "windows")]
fn retry_local_create_operation<T>(
    operation: impl FnMut() -> Result<T, HelperError>,
) -> Result<T, HelperError> {
    retry_operation_until(
        LOCAL_CREATE_IO_TIMEOUT,
        LOCAL_CREATE_IO_RETRY_DELAY,
        operation,
    )
}

#[cfg(target_os = "windows")]
fn retry_operation_until<T>(
    timeout: std::time::Duration,
    delay: std::time::Duration,
    mut operation: impl FnMut() -> Result<T, HelperError>,
) -> Result<T, HelperError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if std::time::Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(delay),
        }
    }
}

#[cfg(target_os = "windows")]
fn run_cloud_filter_provider(
    mount_id: &str,
    sync_root: &Path,
    state_dir: &Path,
) -> Result<(), HelperError> {
    wait_for_daemon(state_dir)?;
    let mut connected = connect_cloud_filter_sync_root(mount_id, sync_root, state_dir)?;
    let seeded = seed_root_placeholders(&connected.context)?;
    connected.local_change_watcher = Some(start_local_change_watcher(
        connected.context.as_ref().clone(),
    )?);
    eprintln!(
        "{COMMAND_NAME}: connected `{mount_id}` at `{}` and seeded {seeded} root placeholder{}",
        sync_root.display(),
        plural(seeded)
    );
    wait_for_shutdown()?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn run_cloud_filter_provider(
    _mount_id: &str,
    _sync_root: &Path,
    _state_dir: &Path,
) -> Result<(), HelperError> {
    Err(HelperError::new(
        "unsupported_platform",
        "Windows Cloud Files provider runtime is only supported on Windows",
    ))
}

#[cfg(target_os = "windows")]
fn wait_for_daemon(state_dir: &Path) -> Result<(), HelperError> {
    let started = std::time::Instant::now();
    let mut last_error = "daemon did not respond".to_string();

    while started.elapsed() < DAEMON_READY_TIMEOUT {
        match afsd::ipc::send_request_with_timeout(
            state_dir,
            &afsd::ipc::DaemonRequest::Ping,
            DAEMON_PING_TIMEOUT,
        ) {
            Ok(response) if response.ok => return Ok(()),
            Ok(response) => {
                last_error = response
                    .error
                    .map(|error| format!("{}: {}", error.code, error.message))
                    .unwrap_or_else(|| "daemon ping failed without an error payload".to_string());
            }
            Err(error) => last_error = error.message().to_string(),
        }
        std::thread::sleep(DAEMON_READY_POLL);
    }

    Err(HelperError::new(
        "daemon_unavailable",
        format!(
            "afsd did not become ready within {}s: {last_error}",
            DAEMON_READY_TIMEOUT.as_secs()
        ),
    ))
}

#[cfg(target_os = "windows")]
fn wait_for_shutdown() -> Result<(), HelperError> {
    let (sender, receiver) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = sender.send(());
    })
    .map_err(|error| HelperError::new("signal_handler_failed", error.to_string()))?;
    receiver
        .recv()
        .map_err(|error| HelperError::new("signal_handler_failed", error.to_string()))
}

#[cfg(target_os = "windows")]
fn connect_cloud_filter_sync_root(
    mount_id: &str,
    sync_root: &Path,
    state_dir: &Path,
) -> Result<ConnectedCloudProvider, HelperError> {
    use windows::Win32::Storage::CloudFilters::{
        CF_CALLBACK_REGISTRATION, CF_CALLBACK_TYPE_FETCH_DATA, CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
        CF_CALLBACK_TYPE_NONE, CF_CALLBACK_TYPE_NOTIFY_DELETE,
        CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION, CF_CALLBACK_TYPE_NOTIFY_RENAME,
        CF_CONNECT_FLAG_NONE, CfConnectSyncRoot,
    };
    use windows::core::PCWSTR;

    let context = Box::new(ProviderContext {
        mount_id: mount_id.to_string(),
        sync_root: sync_root.to_path_buf(),
        state_dir: state_dir.to_path_buf(),
        identity_index: Default::default(),
    });
    let callbacks = [
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
            Callback: Some(on_fetch_placeholders),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_FETCH_DATA,
            Callback: Some(on_fetch_data),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION,
            Callback: Some(on_file_close_completion),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_RENAME,
            Callback: Some(on_rename),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NOTIFY_DELETE,
            Callback: Some(on_delete),
        },
        CF_CALLBACK_REGISTRATION {
            Type: CF_CALLBACK_TYPE_NONE,
            Callback: None,
        },
    ];
    let sync_root_wide = wide_path(sync_root);
    let context_ptr = (&*context) as *const ProviderContext as *const std::ffi::c_void;
    let connection_key = unsafe {
        CfConnectSyncRoot(
            PCWSTR::from_raw(sync_root_wide.as_ptr()),
            callbacks.as_ptr(),
            Some(context_ptr),
            CF_CONNECT_FLAG_NONE,
        )
    }
    .map_err(win32_error("connect cloud filter sync root"))?;

    Ok(ConnectedCloudProvider {
        connection_key,
        context,
        local_change_watcher: None,
    })
}

#[cfg(target_os = "windows")]
fn seed_root_placeholders(context: &ProviderContext) -> Result<usize, HelperError> {
    let children = context.children(afsd::file_provider::ROOT_CONTAINER_IDENTIFIER)?;
    create_placeholders_in_directory(&context.sync_root, &children.children)?;
    remember_placeholder_children(context, &context.sync_root, &children.children);
    Ok(children.children.len())
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn on_fetch_placeholders(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    callback_parameters: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) {
    if let Err(error) = std::panic::catch_unwind(|| {
        let result = unsafe { handle_fetch_placeholders(callback_info, callback_parameters) };
        if let Err(error) = result {
            eprintln!("{COMMAND_NAME}: fetch placeholders failed: {error}");
            unsafe {
                let _ = complete_fetch_placeholders_with_status(
                    callback_info,
                    status_unsuccessful(),
                    std::ptr::null_mut(),
                    0,
                    0,
                );
            }
        }
    }) {
        eprintln!("{COMMAND_NAME}: fetch placeholders panicked: {error:?}");
        unsafe {
            let _ = complete_fetch_placeholders_with_status(
                callback_info,
                status_unsuccessful(),
                std::ptr::null_mut(),
                0,
                0,
            );
        }
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn on_fetch_data(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    callback_parameters: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) {
    if let Err(error) = std::panic::catch_unwind(|| {
        let result = unsafe { handle_fetch_data(callback_info, callback_parameters) };
        if let Err(error) = result {
            eprintln!("{COMMAND_NAME}: fetch data failed: {error}");
            unsafe {
                let _ = complete_fetch_data_with_status(
                    callback_info,
                    status_unsuccessful(),
                    std::ptr::null(),
                    0,
                    0,
                );
            }
        }
    }) {
        eprintln!("{COMMAND_NAME}: fetch data panicked: {error:?}");
        unsafe {
            let _ = complete_fetch_data_with_status(
                callback_info,
                status_unsuccessful(),
                std::ptr::null(),
                0,
                0,
            );
        }
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn on_file_close_completion(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    callback_parameters: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) {
    let _ = callback_parameters;
    if let Err(error) = std::panic::catch_unwind(|| {
        let result = unsafe { handle_file_close_completion(callback_info) };
        if let Err(error) = result {
            eprintln!("{COMMAND_NAME}: close completion failed: {error}");
        }
    }) {
        eprintln!("{COMMAND_NAME}: close completion panicked: {error:?}");
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn on_rename(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    callback_parameters: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) {
    if let Err(error) = std::panic::catch_unwind(|| {
        let result = unsafe { handle_rename(callback_info, callback_parameters) };
        let status = if result.is_ok() {
            status_success()
        } else {
            if let Err(error) = result {
                eprintln!("{COMMAND_NAME}: rename failed: {error}");
            }
            status_unsuccessful()
        };
        unsafe {
            let _ = acknowledge_rename_with_status(callback_info, status);
        }
    }) {
        eprintln!("{COMMAND_NAME}: rename panicked: {error:?}");
        unsafe {
            let _ = acknowledge_rename_with_status(callback_info, status_unsuccessful());
        }
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn on_delete(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    callback_parameters: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) {
    if let Err(error) = std::panic::catch_unwind(|| {
        let result = unsafe { handle_delete(callback_info, callback_parameters) };
        let status = if result.is_ok() {
            status_success()
        } else {
            if let Err(error) = result {
                eprintln!("{COMMAND_NAME}: delete failed: {error}");
            }
            status_unsuccessful()
        };
        unsafe {
            let _ = acknowledge_delete_with_status(callback_info, status);
        }
    }) {
        eprintln!("{COMMAND_NAME}: delete panicked: {error:?}");
        unsafe {
            let _ = acknowledge_delete_with_status(callback_info, status_unsuccessful());
        }
    }
}

#[cfg(target_os = "windows")]
unsafe fn handle_fetch_placeholders(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    _callback_parameters: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) -> Result<(), HelperError> {
    let info = unsafe { callback_info.as_ref() }.ok_or_else(|| {
        HelperError::new(
            "invalid_callback",
            "fetch placeholders callback info was null",
        )
    })?;
    let context = unsafe { provider_context(info) }?;
    let container_identifier = callback_identifier(info)
        .unwrap_or_else(|| afsd::file_provider::ROOT_CONTAINER_IDENTIFIER.to_string());
    trace_cloud_files(format!(
        "fetch placeholders start container=`{container_identifier}`"
    ));
    let children = context.children(&container_identifier)?;
    let directory = callback_path(context, info).unwrap_or_else(|| context.sync_root.clone());
    trace_cloud_files(format!(
        "fetch placeholders transfer container=`{container_identifier}` count={}",
        children.children.len()
    ));
    let mut batch = PlaceholderBatch::from_items(&children.children);
    unsafe {
        complete_fetch_placeholders_with_status(
            callback_info,
            status_success(),
            batch.infos.as_mut_ptr(),
            batch.infos.len() as u32,
            batch.infos.len() as i64,
        )
    }?;
    remember_placeholder_children(context, &directory, &children.children);
    Ok(())
}

#[cfg(target_os = "windows")]
unsafe fn handle_fetch_data(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    callback_parameters: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) -> Result<(), HelperError> {
    let info = unsafe { callback_info.as_ref() }
        .ok_or_else(|| HelperError::new("invalid_callback", "fetch data callback info was null"))?;
    let params = unsafe { callback_parameters.as_ref() }.ok_or_else(|| {
        HelperError::new(
            "invalid_callback",
            "fetch data callback parameters were null",
        )
    })?;
    let context = unsafe { provider_context(info) }?;
    let identifier = callback_identifier(info)
        .ok_or_else(|| HelperError::new("invalid_callback", "fetch data missing file identity"))?;
    let fetch = unsafe { params.Anonymous.FetchData };
    trace_cloud_files(format!(
        "fetch data start identity=`{identifier}` advertised_size={} required_offset={} required_length={}",
        info.FileSize, fetch.RequiredFileOffset, fetch.RequiredLength
    ));
    let read = context.read(&identifier)?;
    let contents = decode_base64(&read.contents_base64)?;
    let content_len = contents.len() as i64;
    trace_cloud_files(format!(
        "fetch data materialized identity=`{identifier}` bytes={content_len} advertised_size={}",
        info.FileSize
    ));

    if info.FileSize != content_len {
        trace_cloud_files(format!(
            "fetch data restart hydration identity=`{identifier}` advertised_size={} materialized_size={content_len}",
            info.FileSize
        ));
        unsafe {
            restart_hydration_with_size(callback_info, &read.item, contents.len(), &identifier)?
        };
        return Ok(());
    }

    let range = required_range(&contents, fetch.RequiredFileOffset, fetch.RequiredLength)?;
    trace_cloud_files(format!(
        "fetch data transfer identity=`{identifier}` offset={} length={}",
        fetch.RequiredFileOffset,
        range.len()
    ));
    unsafe {
        complete_fetch_data_with_status(
            callback_info,
            status_success(),
            range.as_ptr().cast(),
            fetch.RequiredFileOffset,
            range.len() as i64,
        )
    }
}

#[cfg(target_os = "windows")]
unsafe fn handle_file_close_completion(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
) -> Result<(), HelperError> {
    let info = unsafe { callback_info.as_ref() }.ok_or_else(|| {
        HelperError::new(
            "invalid_callback",
            "file close completion callback info was null",
        )
    })?;
    let context = unsafe { provider_context(info) }?;
    let identifier = callback_identifier(info)
        .ok_or_else(|| HelperError::new("invalid_callback", "file close missing file identity"))?;
    let path = callback_path(context, info)
        .ok_or_else(|| HelperError::new("invalid_callback", "file close missing path"))?;
    context.remember_path_identity(&path, &identifier);
    commit_local_file_contents(context, &identifier, &path)
}

#[cfg(target_os = "windows")]
unsafe fn handle_rename(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    callback_parameters: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) -> Result<(), HelperError> {
    use windows::Win32::Storage::CloudFilters::{
        CF_CALLBACK_RENAME_FLAG_SOURCE_IN_SCOPE, CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE,
    };

    let info = unsafe { callback_info.as_ref() }
        .ok_or_else(|| HelperError::new("invalid_callback", "rename callback info was null"))?;
    let params = unsafe { callback_parameters.as_ref() }.ok_or_else(|| {
        HelperError::new("invalid_callback", "rename callback parameters were null")
    })?;
    let context = unsafe { provider_context(info) }?;
    let source_path = callback_path(context, info);
    let rename = unsafe { params.Anonymous.Rename };
    let source_in_scope = rename
        .Flags
        .contains(CF_CALLBACK_RENAME_FLAG_SOURCE_IN_SCOPE);
    let target_in_scope = rename
        .Flags
        .contains(CF_CALLBACK_RENAME_FLAG_TARGET_IN_SCOPE);
    if !source_in_scope {
        return Ok(());
    }

    let mut identifier = callback_identifier(info)
        .ok_or_else(|| HelperError::new("invalid_callback", "rename missing file identity"))?;
    if is_local_identity(&identifier)
        && let Some(source_path) = source_path.as_deref()
        && let Some(refreshed) = daemon_identity_for_path(context, source_path)?
    {
        identifier = refreshed;
    }
    if !target_in_scope {
        context.trash(&identifier)?;
        if let Some(source_path) = source_path.as_deref() {
            context.forget_path_identities(source_path);
        }
        return Ok(());
    }

    let target_path = pcwstr_to_path(rename.TargetPath)
        .ok_or_else(|| HelperError::new("invalid_callback", "rename missing target path"))?;
    let target_path = absolute_cloud_path(context, &target_path);
    let new_filename = target_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| HelperError::new("invalid_callback", "rename target missing filename"))?;
    let new_parent_path = target_path
        .parent()
        .ok_or_else(|| HelperError::new("invalid_callback", "rename target missing parent"))?;
    let new_parent_identifier = parent_identifier_for_path(context, new_parent_path)?;
    context.rename(&identifier, &new_parent_identifier, new_filename)?;
    if let Some(source_path) = source_path.as_deref() {
        context.move_path_identities(source_path, &target_path);
    }
    context.remember_path_identity(&target_path, &identifier);
    Ok(())
}

#[cfg(target_os = "windows")]
unsafe fn handle_delete(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    callback_parameters: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_PARAMETERS,
) -> Result<(), HelperError> {
    use windows::Win32::Storage::CloudFilters::CF_CALLBACK_DELETE_FLAG_IS_UNDELETE;

    let info = unsafe { callback_info.as_ref() }
        .ok_or_else(|| HelperError::new("invalid_callback", "delete callback info was null"))?;
    let params = unsafe { callback_parameters.as_ref() }.ok_or_else(|| {
        HelperError::new("invalid_callback", "delete callback parameters were null")
    })?;
    let delete = unsafe { params.Anonymous.Delete };
    if delete.Flags.contains(CF_CALLBACK_DELETE_FLAG_IS_UNDELETE) {
        return Err(HelperError::new(
            "unsupported_delete",
            "Windows Cloud Files undelete notifications are not supported yet",
        ));
    }

    let context = unsafe { provider_context(info) }?;
    let path = callback_path(context, info);
    let mut identifier = callback_identifier(info)
        .ok_or_else(|| HelperError::new("invalid_callback", "delete missing file identity"))?;
    if is_local_identity(&identifier)
        && let Some(path) = path.as_deref()
        && let Some(refreshed) = daemon_identity_for_path(context, path)?
    {
        identifier = refreshed;
    }
    match context.trash(&identifier) {
        Ok(_) => {
            if let Some(path) = path.as_deref() {
                context.forget_path_identities(path);
            }
            Ok(())
        }
        Err(error) if stale_pending_page_directory_delete(&identifier, &error) => {
            if let Some(path) = path.as_deref() {
                context.forget_path_identities(path);
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "windows")]
fn stale_pending_page_directory_delete(identifier: &str, error: &HelperError) -> bool {
    identifier.starts_with("children:local:")
        && error.message.contains("not present in daemon state")
}

#[cfg(target_os = "windows")]
unsafe fn provider_context(
    info: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
) -> Result<&'static ProviderContext, HelperError> {
    let context = info.CallbackContext as *const ProviderContext;
    unsafe { context.as_ref() }
        .ok_or_else(|| HelperError::new("invalid_callback", "callback context was null"))
}

#[cfg(target_os = "windows")]
fn callback_identifier(
    info: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
) -> Option<String> {
    if info.FileIdentity.is_null() || info.FileIdentityLength == 0 {
        return None;
    }
    let bytes = unsafe {
        std::slice::from_raw_parts(
            info.FileIdentity.cast::<u8>(),
            info.FileIdentityLength as usize,
        )
    };
    String::from_utf8(bytes.to_vec()).ok()
}

#[cfg(target_os = "windows")]
fn callback_path(
    context: &ProviderContext,
    info: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
) -> Option<PathBuf> {
    let path = pcwstr_to_path(info.NormalizedPath)?;
    Some(absolute_cloud_path(context, &path))
}

#[cfg(target_os = "windows")]
fn commit_local_file_contents(
    context: &ProviderContext,
    identifier: &str,
    path: &Path,
) -> Result<(), HelperError> {
    if !path_is_under_sync_root(context, path) {
        return Ok(());
    }
    let Some(info) = placeholder_info_for_path(path)? else {
        return Ok(());
    };
    if info.in_sync {
        return Ok(());
    }
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(HelperError::io("inspect closed file", error)),
    };
    if !metadata.is_file() {
        return Ok(());
    }
    let contents =
        std::fs::read(path).map_err(|error| HelperError::io("read closed file", error))?;
    commit_local_bytes(context, identifier, path, &contents)
}

#[cfg(target_os = "windows")]
fn commit_local_bytes(
    context: &ProviderContext,
    identifier: &str,
    path: &Path,
    contents: &[u8],
) -> Result<(), HelperError> {
    let report = context.commit_write(identifier, contents)?;
    let in_sync = report.hydration == afs_core::model::HydrationState::Hydrated;
    let _ = set_placeholder_in_sync_state(path, in_sync);
    Ok(())
}

#[cfg(target_os = "windows")]
fn parent_identifier_for_path(
    context: &ProviderContext,
    parent: &Path,
) -> Result<String, HelperError> {
    let parent = absolute_cloud_path(context, parent);
    if same_cloud_path(&parent, &context.sync_root) {
        return Ok(afsd::file_provider::ROOT_CONTAINER_IDENTIFIER.to_string());
    }
    if let Some(identifier) = identity_for_path(context, &parent)? {
        return Ok(identifier);
    }
    Err(HelperError::new(
        "missing_parent_identity",
        format!(
            "could not resolve Cloud Files identity for parent `{}`",
            parent.display()
        ),
    ))
}

#[cfg(target_os = "windows")]
fn placeholder_identity_for_path(path: &Path) -> Result<Option<String>, HelperError> {
    Ok(placeholder_info_for_path(path)?.map(|info| info.identity))
}

#[cfg(target_os = "windows")]
struct PlaceholderInfo {
    identity: String,
    in_sync: bool,
}

#[cfg(target_os = "windows")]
fn placeholder_info_for_path(path: &Path) -> Result<Option<PlaceholderInfo>, HelperError> {
    use windows::Win32::Storage::CloudFilters::{
        CF_IN_SYNC_STATE_IN_SYNC, CF_PLACEHOLDER_BASIC_INFO, CF_PLACEHOLDER_INFO_BASIC,
        CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH, CfGetPlaceholderInfo,
    };

    let handle = match open_cloud_file(
        path,
        windows::Win32::Storage::CloudFilters::CF_OPEN_FILE_FLAG_NONE,
    ) {
        Ok(handle) => handle,
        Err(_) => return Ok(None),
    };
    let mut buffer = vec![
        0_u8;
        std::mem::size_of::<CF_PLACEHOLDER_BASIC_INFO>()
            + CF_PLACEHOLDER_MAX_FILE_IDENTITY_LENGTH as usize
    ];
    let mut returned = 0_u32;
    if unsafe {
        CfGetPlaceholderInfo(
            handle.raw(),
            CF_PLACEHOLDER_INFO_BASIC,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            Some(&mut returned),
        )
    }
    .is_err()
    {
        return Ok(None);
    }

    let info = unsafe { &*(buffer.as_ptr().cast::<CF_PLACEHOLDER_BASIC_INFO>()) };
    let identity_length = info.FileIdentityLength as usize;
    if identity_length == 0 {
        return Ok(None);
    }
    let identity_offset = std::mem::offset_of!(CF_PLACEHOLDER_BASIC_INFO, FileIdentity);
    if identity_offset + identity_length > buffer.len() {
        return Err(HelperError::new(
            "invalid_placeholder",
            format!(
                "placeholder identity for `{}` exceeded buffer",
                path.display()
            ),
        ));
    }
    let identity =
        String::from_utf8(buffer[identity_offset..identity_offset + identity_length].to_vec())
            .map_err(|error| HelperError::new("invalid_placeholder", error.to_string()))?;
    Ok(Some(PlaceholderInfo {
        identity,
        in_sync: info.InSyncState == CF_IN_SYNC_STATE_IN_SYNC,
    }))
}

#[cfg(target_os = "windows")]
fn convert_to_placeholder(path: &Path, identifier: &str, in_sync: bool) -> Result<(), HelperError> {
    use windows::Win32::Storage::CloudFilters::{
        CF_CONVERT_FLAG_MARK_IN_SYNC, CF_CONVERT_FLAG_NONE, CF_OPEN_FILE_FLAG_EXCLUSIVE,
        CF_OPEN_FILE_FLAG_WRITE_ACCESS, CfConvertToPlaceholder,
    };

    let handle = open_cloud_file(
        path,
        CF_OPEN_FILE_FLAG_WRITE_ACCESS | CF_OPEN_FILE_FLAG_EXCLUSIVE,
    )?;
    let identity = identifier.as_bytes();
    let flags = if in_sync {
        CF_CONVERT_FLAG_MARK_IN_SYNC
    } else {
        CF_CONVERT_FLAG_NONE
    };
    unsafe {
        CfConvertToPlaceholder(
            handle.raw(),
            Some(identity.as_ptr().cast()),
            identity.len() as u32,
            flags,
            None,
            None,
        )
    }
    .map_err(win32_error("convert local item to cloud placeholder"))
}

#[cfg(target_os = "windows")]
fn set_placeholder_in_sync_state(path: &Path, in_sync: bool) -> Result<(), HelperError> {
    use windows::Win32::Storage::CloudFilters::{
        CF_IN_SYNC_STATE_IN_SYNC, CF_IN_SYNC_STATE_NOT_IN_SYNC, CF_OPEN_FILE_FLAG_WRITE_ACCESS,
        CF_SET_IN_SYNC_FLAG_NONE, CfSetInSyncState,
    };

    let handle = open_cloud_file(path, CF_OPEN_FILE_FLAG_WRITE_ACCESS)?;
    let state = if in_sync {
        CF_IN_SYNC_STATE_IN_SYNC
    } else {
        CF_IN_SYNC_STATE_NOT_IN_SYNC
    };
    unsafe { CfSetInSyncState(handle.raw(), state, CF_SET_IN_SYNC_FLAG_NONE, None) }
        .map_err(win32_error("set cloud placeholder sync state"))
}

#[cfg(target_os = "windows")]
struct CloudFileHandle(windows::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
impl CloudFileHandle {
    fn raw(&self) -> windows::Win32::Foundation::HANDLE {
        self.0
    }
}

#[cfg(target_os = "windows")]
impl Drop for CloudFileHandle {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::Storage::CloudFilters::CfCloseHandle(self.0);
        }
    }
}

#[cfg(target_os = "windows")]
fn open_cloud_file(
    path: &Path,
    flags: windows::Win32::Storage::CloudFilters::CF_OPEN_FILE_FLAGS,
) -> Result<CloudFileHandle, HelperError> {
    use windows::Win32::Storage::CloudFilters::CfOpenFileWithOplock;
    use windows::core::PCWSTR;

    let path_wide = wide_path(path);
    unsafe { CfOpenFileWithOplock(PCWSTR::from_raw(path_wide.as_ptr()), flags) }
        .map(CloudFileHandle)
        .map_err(win32_error("open cloud file"))
}

#[cfg(target_os = "windows")]
fn remember_placeholder_children(
    context: &ProviderContext,
    directory: &Path,
    items: &[afsd::file_provider::FileProviderItem],
) {
    for item in items {
        context.remember_path_identity(&directory.join(&item.filename), &item.identifier);
    }
}

#[cfg(target_os = "windows")]
fn create_placeholders_in_directory(
    directory: &Path,
    items: &[afsd::file_provider::FileProviderItem],
) -> Result<(), HelperError> {
    use windows::Win32::Storage::CloudFilters::{CF_CREATE_FLAG_NONE, CfCreatePlaceholders};
    use windows::core::PCWSTR;

    if items.is_empty() {
        return Ok(());
    }

    let mut missing_items = Vec::with_capacity(items.len());
    for item in items {
        let placeholder_path = directory.join(&item.filename);
        match placeholder_path.try_exists() {
            Ok(true) => {}
            Ok(false) => missing_items.push(item.clone()),
            Err(error) => {
                return Err(HelperError::io("inspect cloud file placeholder", error));
            }
        }
    }

    if missing_items.is_empty() {
        trace_cloud_files(format!(
            "create placeholders skipped directory=`{}` existing_count={}",
            directory.display(),
            items.len()
        ));
        return Ok(());
    }

    trace_cloud_files(format!(
        "create placeholders directory=`{}` missing_count={} requested_count={}",
        directory.display(),
        missing_items.len(),
        items.len()
    ));
    let directory_wide = wide_path(directory);
    let mut batch = PlaceholderBatch::from_items(&missing_items);
    unsafe {
        CfCreatePlaceholders(
            PCWSTR::from_raw(directory_wide.as_ptr()),
            &mut batch.infos,
            CF_CREATE_FLAG_NONE,
            None,
        )
    }
    .map_err(win32_error("create cloud file placeholders"))
}

#[cfg(target_os = "windows")]
struct PlaceholderBatch {
    _names: Vec<Vec<u16>>,
    _identities: Vec<Vec<u8>>,
    infos: Vec<windows::Win32::Storage::CloudFilters::CF_PLACEHOLDER_CREATE_INFO>,
}

#[cfg(target_os = "windows")]
impl PlaceholderBatch {
    fn from_items(items: &[afsd::file_provider::FileProviderItem]) -> Self {
        use windows::Win32::Storage::CloudFilters::{
            CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC, CF_PLACEHOLDER_CREATE_FLAG_SUPERSEDE,
            CF_PLACEHOLDER_CREATE_INFO,
        };
        use windows::core::{HRESULT, PCWSTR};

        let mut names = Vec::with_capacity(items.len());
        let mut identities = Vec::with_capacity(items.len());
        let mut infos = Vec::with_capacity(items.len());

        for item in items {
            names.push(wide_str(&item.filename));
            identities.push(item.identifier.as_bytes().to_vec());
            let name = names.last().expect("placeholder name").as_ptr();
            let identity = identities.last().expect("placeholder identity");
            infos.push(CF_PLACEHOLDER_CREATE_INFO {
                RelativeFileName: PCWSTR::from_raw(name),
                FsMetadata: fs_metadata_for_item(item, placeholder_size_for_item(item)),
                FileIdentity: identity.as_ptr().cast(),
                FileIdentityLength: identity.len() as u32,
                Flags: CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC
                    | CF_PLACEHOLDER_CREATE_FLAG_SUPERSEDE,
                Result: HRESULT(0),
                CreateUsn: 0,
            });
        }

        Self {
            _names: names,
            _identities: identities,
            infos,
        }
    }
}

#[cfg(target_os = "windows")]
fn fs_metadata_for_item(
    item: &afsd::file_provider::FileProviderItem,
    size: usize,
) -> windows::Win32::Storage::CloudFilters::CF_FS_METADATA {
    use windows::Win32::Storage::CloudFilters::CF_FS_METADATA;
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_BASIC_INFO,
    };

    let attributes = if item.kind == afsd::file_provider::FileProviderItemKind::Folder {
        FILE_ATTRIBUTE_DIRECTORY.0
    } else {
        FILE_ATTRIBUTE_NORMAL.0
    };

    CF_FS_METADATA {
        BasicInfo: FILE_BASIC_INFO {
            FileAttributes: attributes,
            ..Default::default()
        },
        FileSize: size as i64,
    }
}

#[cfg(target_os = "windows")]
fn placeholder_size_for_item(item: &afsd::file_provider::FileProviderItem) -> usize {
    if item.kind == afsd::file_provider::FileProviderItemKind::Folder {
        0
    } else {
        item.byte_size.unwrap_or(1).max(1) as usize
    }
}

#[cfg(target_os = "windows")]
unsafe fn complete_fetch_placeholders_with_status(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    status: windows::Win32::Foundation::NTSTATUS,
    placeholders: *mut windows::Win32::Storage::CloudFilters::CF_PLACEHOLDER_CREATE_INFO,
    placeholder_count: u32,
    placeholder_total_count: i64,
) -> Result<(), HelperError> {
    use windows::Win32::Storage::CloudFilters::{
        CF_OPERATION_PARAMETERS, CF_OPERATION_PARAMETERS_0, CF_OPERATION_PARAMETERS_0_4,
        CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS, CfExecute,
    };

    let info = unsafe { callback_info.as_ref() }.ok_or_else(|| {
        HelperError::new(
            "invalid_callback",
            "fetch placeholders completion callback info was null",
        )
    })?;
    let operation_info = operation_info(info, CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS);
    let flags = transfer_placeholders_flags_for_status(status);
    let mut parameters = CF_OPERATION_PARAMETERS {
        ParamSize: operation_parameter_size::<CF_OPERATION_PARAMETERS_0_4>(),
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferPlaceholders: CF_OPERATION_PARAMETERS_0_4 {
                Flags: flags,
                CompletionStatus: status,
                PlaceholderTotalCount: placeholder_total_count,
                PlaceholderArray: placeholders,
                PlaceholderCount: placeholder_count,
                EntriesProcessed: placeholder_count,
            },
        },
    };

    trace_cloud_files(format!(
        "complete fetch placeholders execute count={placeholder_count} total={placeholder_total_count} entries_processed={placeholder_count}"
    ));
    let result = unsafe { CfExecute(&operation_info, &mut parameters) };
    trace_cloud_files(format!(
        "complete fetch placeholders returned count={placeholder_count} result={result:?}"
    ));
    result.map_err(win32_error("complete fetch placeholders"))
}

#[cfg(target_os = "windows")]
fn transfer_placeholders_flags_for_status(
    status: windows::Win32::Foundation::NTSTATUS,
) -> windows::Win32::Storage::CloudFilters::CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAGS {
    use windows::Win32::Storage::CloudFilters::{
        CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
        CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_NONE,
    };

    if status.0 == STATUS_SUCCESS_VALUE {
        CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION
    } else {
        CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_NONE
    }
}

#[cfg(target_os = "windows")]
unsafe fn complete_fetch_data_with_status(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    status: windows::Win32::Foundation::NTSTATUS,
    buffer: *const std::ffi::c_void,
    offset: i64,
    length: i64,
) -> Result<(), HelperError> {
    use windows::Win32::Storage::CloudFilters::{
        CF_OPERATION_PARAMETERS, CF_OPERATION_PARAMETERS_0, CF_OPERATION_PARAMETERS_0_0,
        CF_OPERATION_TRANSFER_DATA_FLAG_NONE, CF_OPERATION_TYPE_TRANSFER_DATA, CfExecute,
    };

    let info = unsafe { callback_info.as_ref() }.ok_or_else(|| {
        HelperError::new(
            "invalid_callback",
            "fetch data completion callback info was null",
        )
    })?;
    let operation_info = operation_info(info, CF_OPERATION_TYPE_TRANSFER_DATA);
    let mut parameters = CF_OPERATION_PARAMETERS {
        ParamSize: operation_parameter_size::<CF_OPERATION_PARAMETERS_0_0>(),
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            TransferData: CF_OPERATION_PARAMETERS_0_0 {
                Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
                CompletionStatus: status,
                Buffer: buffer,
                Offset: offset,
                Length: length,
            },
        },
    };

    trace_cloud_files(format!(
        "complete fetch data execute offset={offset} length={length} status={}",
        status.0
    ));
    let result = unsafe { CfExecute(&operation_info, &mut parameters) };
    trace_cloud_files(format!(
        "complete fetch data returned offset={offset} length={length} result={result:?}"
    ));
    result.map_err(win32_error("complete fetch data"))
}

#[cfg(target_os = "windows")]
unsafe fn acknowledge_delete_with_status(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    status: windows::Win32::Foundation::NTSTATUS,
) -> Result<(), HelperError> {
    use windows::Win32::Storage::CloudFilters::{
        CF_OPERATION_ACK_DELETE_FLAG_NONE, CF_OPERATION_PARAMETERS, CF_OPERATION_PARAMETERS_0,
        CF_OPERATION_PARAMETERS_0_7, CF_OPERATION_TYPE_ACK_DELETE, CfExecute,
    };

    let info = unsafe { callback_info.as_ref() }.ok_or_else(|| {
        HelperError::new(
            "invalid_callback",
            "delete acknowledgement callback info was null",
        )
    })?;
    let operation_info = operation_info(info, CF_OPERATION_TYPE_ACK_DELETE);
    let mut parameters = CF_OPERATION_PARAMETERS {
        ParamSize: operation_parameter_size::<CF_OPERATION_PARAMETERS_0_7>(),
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            AckDelete: CF_OPERATION_PARAMETERS_0_7 {
                Flags: CF_OPERATION_ACK_DELETE_FLAG_NONE,
                CompletionStatus: status,
            },
        },
    };

    unsafe { CfExecute(&operation_info, &mut parameters) }
        .map_err(win32_error("acknowledge delete"))
}

#[cfg(target_os = "windows")]
unsafe fn acknowledge_rename_with_status(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    status: windows::Win32::Foundation::NTSTATUS,
) -> Result<(), HelperError> {
    use windows::Win32::Storage::CloudFilters::{
        CF_OPERATION_ACK_RENAME_FLAG_NONE, CF_OPERATION_PARAMETERS, CF_OPERATION_PARAMETERS_0,
        CF_OPERATION_PARAMETERS_0_6, CF_OPERATION_TYPE_ACK_RENAME, CfExecute,
    };

    let info = unsafe { callback_info.as_ref() }.ok_or_else(|| {
        HelperError::new(
            "invalid_callback",
            "rename acknowledgement callback info was null",
        )
    })?;
    let operation_info = operation_info(info, CF_OPERATION_TYPE_ACK_RENAME);
    let mut parameters = CF_OPERATION_PARAMETERS {
        ParamSize: operation_parameter_size::<CF_OPERATION_PARAMETERS_0_6>(),
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            AckRename: CF_OPERATION_PARAMETERS_0_6 {
                Flags: CF_OPERATION_ACK_RENAME_FLAG_NONE,
                CompletionStatus: status,
            },
        },
    };

    unsafe { CfExecute(&operation_info, &mut parameters) }
        .map_err(win32_error("acknowledge rename"))
}

#[cfg(target_os = "windows")]
unsafe fn restart_hydration_with_size(
    callback_info: *const windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    item: &afsd::file_provider::FileProviderItem,
    size: usize,
    identifier: &str,
) -> Result<(), HelperError> {
    use windows::Win32::Storage::CloudFilters::{
        CF_OPERATION_PARAMETERS, CF_OPERATION_PARAMETERS_0, CF_OPERATION_PARAMETERS_0_3,
        CF_OPERATION_RESTART_HYDRATION_FLAG_NONE, CF_OPERATION_TYPE_RESTART_HYDRATION, CfExecute,
    };

    let info = unsafe { callback_info.as_ref() }.ok_or_else(|| {
        HelperError::new(
            "invalid_callback",
            "restart hydration callback info was null",
        )
    })?;
    let identity = identifier.as_bytes();
    let metadata = fs_metadata_for_item(item, size);
    let operation_info = operation_info(info, CF_OPERATION_TYPE_RESTART_HYDRATION);
    let mut parameters = CF_OPERATION_PARAMETERS {
        ParamSize: operation_parameter_size::<CF_OPERATION_PARAMETERS_0_3>(),
        Anonymous: CF_OPERATION_PARAMETERS_0 {
            RestartHydration: CF_OPERATION_PARAMETERS_0_3 {
                Flags: CF_OPERATION_RESTART_HYDRATION_FLAG_NONE,
                FsMetadata: &metadata,
                FileIdentity: identity.as_ptr().cast(),
                FileIdentityLength: identity.len() as u32,
            },
        },
    };

    unsafe { CfExecute(&operation_info, &mut parameters) }
        .map_err(win32_error("restart hydration with materialized size"))
}

#[cfg(target_os = "windows")]
fn operation_info(
    callback_info: &windows::Win32::Storage::CloudFilters::CF_CALLBACK_INFO,
    operation_type: windows::Win32::Storage::CloudFilters::CF_OPERATION_TYPE,
) -> windows::Win32::Storage::CloudFilters::CF_OPERATION_INFO {
    windows::Win32::Storage::CloudFilters::CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<windows::Win32::Storage::CloudFilters::CF_OPERATION_INFO>()
            as u32,
        Type: operation_type,
        ConnectionKey: callback_info.ConnectionKey,
        TransferKey: callback_info.TransferKey,
        CorrelationVector: callback_info.CorrelationVector,
        SyncStatus: std::ptr::null(),
        RequestKey: callback_info.RequestKey,
    }
}

#[cfg(target_os = "windows")]
fn operation_parameter_size<T>() -> u32 {
    (std::mem::offset_of!(
        windows::Win32::Storage::CloudFilters::CF_OPERATION_PARAMETERS,
        Anonymous
    ) + std::mem::size_of::<T>()) as u32
}

#[cfg(target_os = "windows")]
fn status_success() -> windows::Win32::Foundation::NTSTATUS {
    windows::Win32::Foundation::NTSTATUS(STATUS_SUCCESS_VALUE)
}

#[cfg(target_os = "windows")]
fn status_unsuccessful() -> windows::Win32::Foundation::NTSTATUS {
    windows::Win32::Foundation::NTSTATUS(STATUS_UNSUCCESSFUL_VALUE)
}

#[cfg(target_os = "windows")]
fn decode_daemon_response<T>(response: afsd::ipc::DaemonResponse) -> Result<T, HelperError>
where
    T: serde::de::DeserializeOwned,
{
    if let Some(error) = response.error {
        return Err(HelperError::new(
            "daemon_error",
            format!("{}: {}", error.code, error.message),
        ));
    }
    let payload = response
        .payload
        .ok_or_else(|| HelperError::new("daemon_error", "daemon returned no payload"))?;
    serde_json::from_value(payload)
        .map_err(|error| HelperError::new("daemon_error", error.to_string()))
}

#[cfg(target_os = "windows")]
fn decode_base64(value: &str) -> Result<Vec<u8>, HelperError> {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;

    BASE64
        .decode(value)
        .map_err(|error| HelperError::new("daemon_error", error.to_string()))
}

#[cfg(target_os = "windows")]
fn required_range(contents: &[u8], offset: i64, length: i64) -> Result<&[u8], HelperError> {
    if offset < 0 || length < 0 {
        return Err(HelperError::new(
            "invalid_callback",
            format!("invalid requested data range offset={offset} length={length}"),
        ));
    }
    let start = offset as usize;
    if start >= contents.len() {
        return Ok(&[]);
    }
    let end = start.saturating_add(length as usize).min(contents.len());
    Ok(&contents[start..end])
}

#[cfg(target_os = "windows")]
fn pcwstr_to_path(value: windows::core::PCWSTR) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;

    if value.is_null() {
        return None;
    }
    let mut len = 0_usize;
    unsafe {
        while *value.0.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(value.0, len);
        Some(PathBuf::from(std::ffi::OsString::from_wide(slice)))
    }
}

#[cfg(target_os = "windows")]
fn absolute_cloud_path(context: &ProviderContext, path: &Path) -> PathBuf {
    let path = platform_display_path(path.to_path_buf());
    if path_has_drive_or_unc_prefix(&path) {
        return path;
    }
    if path.is_absolute()
        && let Some(prefixed) = drive_relative_to_sync_root(context, &path)
    {
        return prefixed;
    }
    if path.is_absolute() {
        return path;
    }
    context.sync_root.join(path)
}

#[cfg(target_os = "windows")]
fn path_has_drive_or_unc_prefix(path: &Path) -> bool {
    use std::path::Component;

    matches!(path.components().next(), Some(Component::Prefix(_)))
}

#[cfg(target_os = "windows")]
fn drive_relative_to_sync_root(context: &ProviderContext, path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    let Some(Component::Prefix(prefix)) = context.sync_root.components().next() else {
        return None;
    };
    let prefix = prefix.as_os_str().to_string_lossy();
    let path = path.as_os_str().to_string_lossy();
    Some(PathBuf::from(format!("{prefix}{path}")))
}

#[cfg(target_os = "windows")]
fn path_is_under_sync_root(context: &ProviderContext, path: &Path) -> bool {
    let path = normalized_cloud_path_string(path);
    let root = normalized_cloud_path_string(&context.sync_root);
    path == root || path.starts_with(&(root + r"\"))
}

#[cfg(target_os = "windows")]
fn same_cloud_path(left: &Path, right: &Path) -> bool {
    normalized_cloud_path_string(left) == normalized_cloud_path_string(right)
}

#[cfg(target_os = "windows")]
fn normalized_cloud_path_string(path: &Path) -> String {
    let path = platform_display_path(path.to_path_buf());
    path.to_string_lossy()
        .replace('/', r"\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

#[cfg(target_os = "windows")]
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(target_os = "windows")]
fn wide_str(value: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(target_os = "windows")]
fn register_shell_sync_root(
    sync_root_id: &str,
    display_name: &str,
    sync_root: &Path,
) -> Result<(), HelperError> {
    use windows::Storage::Provider::{
        StorageProviderHardlinkPolicy, StorageProviderHydrationPolicy,
        StorageProviderHydrationPolicyModifier, StorageProviderInSyncPolicy,
        StorageProviderPopulationPolicy, StorageProviderProtectionMode,
        StorageProviderSyncRootInfo, StorageProviderSyncRootManager,
    };
    use windows::Storage::StorageFolder;
    use windows::core::{GUID, HSTRING};

    if !StorageProviderSyncRootManager::IsSupported().map_err(winrt_error("check support"))? {
        return Err(HelperError::new(
            "unsupported_platform",
            "Windows StorageProviderSyncRootManager is not supported on this system",
        ));
    }

    let folder = StorageFolder::GetFolderFromPathAsync(&HSTRING::from(sync_root))
        .map_err(winrt_error("resolve sync root folder"))?
        .get()
        .map_err(winrt_error("resolve sync root folder"))?;

    let info = StorageProviderSyncRootInfo::new().map_err(winrt_error("create sync root info"))?;
    info.SetId(&HSTRING::from(sync_root_id))
        .map_err(winrt_error("set sync root id"))?;
    info.SetPath(&folder)
        .map_err(winrt_error("set sync root path"))?;
    info.SetDisplayNameResource(&HSTRING::from(display_name))
        .map_err(winrt_error("set display name"))?;
    info.SetIconResource(&provider_icon_resource())
        .map_err(winrt_error("set icon resource"))?;
    info.SetHydrationPolicy(StorageProviderHydrationPolicy::Partial)
        .map_err(winrt_error("set hydration policy"))?;
    info.SetHydrationPolicyModifier(
        StorageProviderHydrationPolicyModifier::StreamingAllowed
            | StorageProviderHydrationPolicyModifier::AllowFullRestartHydration
            | StorageProviderHydrationPolicyModifier::AutoDehydrationAllowed,
    )
    .map_err(winrt_error("set hydration modifier"))?;
    info.SetPopulationPolicy(StorageProviderPopulationPolicy::Full)
        .map_err(winrt_error("set population policy"))?;
    info.SetInSyncPolicy(
        StorageProviderInSyncPolicy::FileCreationTime
            | StorageProviderInSyncPolicy::DirectoryCreationTime
            | StorageProviderInSyncPolicy::FileLastWriteTime
            | StorageProviderInSyncPolicy::DirectoryLastWriteTime,
    )
    .map_err(winrt_error("set in-sync policy"))?;
    info.SetHardlinkPolicy(StorageProviderHardlinkPolicy::None)
        .map_err(winrt_error("set hardlink policy"))?;
    info.SetShowSiblingsAsGroup(false)
        .map_err(winrt_error("set sibling grouping"))?;
    info.SetVersion(&HSTRING::from(env!("CARGO_PKG_VERSION")))
        .map_err(winrt_error("set provider version"))?;
    info.SetProtectionMode(StorageProviderProtectionMode::Personal)
        .map_err(winrt_error("set protection mode"))?;
    info.SetAllowPinning(true)
        .map_err(winrt_error("set pinning policy"))?;
    info.SetProviderId(GUID::from_u128(PROVIDER_GUID))
        .map_err(winrt_error("set provider id"))?;

    let _ = StorageProviderSyncRootManager::Unregister(&HSTRING::from(sync_root_id));
    StorageProviderSyncRootManager::Register(&info).map_err(winrt_error("register sync root"))
}

#[cfg(not(target_os = "windows"))]
fn register_shell_sync_root(
    _sync_root_id: &str,
    _display_name: &str,
    _sync_root: &Path,
) -> Result<(), HelperError> {
    Err(HelperError::new(
        "unsupported_platform",
        "Windows Cloud Files registration is only supported on Windows",
    ))
}

#[cfg(target_os = "windows")]
fn unregister_shell_sync_root(sync_root_id: &str) -> Result<(), HelperError> {
    use windows::Storage::Provider::StorageProviderSyncRootManager;
    use windows::core::HSTRING;

    StorageProviderSyncRootManager::Unregister(&HSTRING::from(sync_root_id))
        .map_err(winrt_error("unregister sync root"))
}

#[cfg(not(target_os = "windows"))]
fn unregister_shell_sync_root(_sync_root_id: &str) -> Result<(), HelperError> {
    Err(HelperError::new(
        "unsupported_platform",
        "Windows Cloud Files unregister is only supported on Windows",
    ))
}

#[cfg(target_os = "windows")]
fn list_shell_sync_roots() -> Result<Vec<SyncRootReport>, HelperError> {
    use windows::Storage::Provider::StorageProviderSyncRootManager;

    let roots =
        StorageProviderSyncRootManager::GetCurrentSyncRoots().map_err(winrt_error("list roots"))?;
    let mut reports = Vec::new();
    for index in 0..roots.Size().map_err(winrt_error("count roots"))? {
        let root = roots.GetAt(index).map_err(winrt_error("read root"))?;
        let id = root.Id().map_err(winrt_error("read root id"))?.to_string();
        if !id.starts_with(SYNC_ROOT_ID_PREFIX) {
            continue;
        }
        let path = root
            .Path()
            .and_then(|folder| folder.Path())
            .map(|path| path.to_string())
            .ok();
        let display_name = root.DisplayNameResource().map(|name| name.to_string()).ok();
        let version = root.Version().map(|version| version.to_string()).ok();
        reports.push(SyncRootReport {
            mount_id: mount_id_from_sync_root_id(&id),
            id,
            display_name,
            path,
            version,
        });
    }
    Ok(reports)
}

#[cfg(not(target_os = "windows"))]
fn list_shell_sync_roots() -> Result<Vec<SyncRootReport>, HelperError> {
    Err(HelperError::new(
        "unsupported_platform",
        "Windows Cloud Files listing is only supported on Windows",
    ))
}

fn open_sync_root(sync_root: &Path) -> Result<(), HelperError> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer.exe")
            .arg(sync_root)
            .spawn()
            .map_err(|error| HelperError::io("open sync root", error))?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = sync_root;
        Err(HelperError::new(
            "unsupported_platform",
            "Windows Cloud Files opening is only supported on Windows",
        ))
    }
}

#[cfg(target_os = "windows")]
fn provider_icon_resource() -> windows::core::HSTRING {
    let icon_resource = std::env::current_exe()
        .ok()
        .map(|path| format!("{},0", path.display()))
        .unwrap_or_else(|| "shell32.dll,-16739".to_string());
    windows::core::HSTRING::from(icon_resource)
}

#[cfg(target_os = "windows")]
fn winrt_error(
    context: &'static str,
) -> impl FnOnce(windows::core::Error) -> HelperError + 'static {
    move |error| HelperError::new("cloud_files_error", format!("{context}: {error}"))
}

#[cfg(target_os = "windows")]
fn win32_error(
    context: &'static str,
) -> impl FnOnce(windows::core::Error) -> HelperError + 'static {
    move |error| HelperError::new("cloud_filter_error", format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_root_ids_encode_mount_ids_losslessly() {
        let mount_id = "notion/main docs!";
        let sync_root_id = sync_root_id_for_mount(mount_id);

        assert_eq!(
            sync_root_id,
            "codeflash.ai.afs!default!notion%2Fmain%20docs%21"
        );
        assert_eq!(
            mount_id_from_sync_root_id(&sync_root_id).as_deref(),
            Some(mount_id)
        );
    }

    #[test]
    fn invalid_sync_root_ids_do_not_decode() {
        assert_eq!(mount_id_from_sync_root_id("other!root"), None);
        assert_eq!(
            mount_id_from_sync_root_id("codeflash.ai.afs!default!bad%XX"),
            None
        );
    }

    #[test]
    fn marker_paths_escape_mount_ids() {
        assert_eq!(
            registration_marker_dir(Path::new(r"C:\State"), "notion/main"),
            PathBuf::from(r"C:\State")
                .join("cloud-files")
                .join("notion%2Fmain")
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn placeholders_without_known_sizes_stay_online_only() {
        let item = afsd::file_provider::FileProviderItem {
            identifier: "notion-main:page-1".to_string(),
            parent_identifier: Some(afsd::file_provider::ROOT_CONTAINER_IDENTIFIER.to_string()),
            filename: "Roadmap.md".to_string(),
            kind: afsd::file_provider::FileProviderItemKind::File,
            entity_kind: None,
            remote_id: Some("page-1".to_string()),
            path: "Notion/Roadmap.md".to_string(),
            hydration: None,
            content_type: "text/markdown".to_string(),
            remote_edited_at: None,
            materialized_path: None,
            byte_size: None,
        };

        assert_eq!(placeholder_size_for_item(&item), 1);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn watcher_modify_events_ignore_metadata_and_non_write_closes() {
        use notify::event::{
            AccessKind, AccessMode, DataChange, EventKind, MetadataKind, ModifyKind,
        };

        assert!(is_modify_like_event(&EventKind::Modify(ModifyKind::Data(
            DataChange::Content
        ))));
        assert!(is_modify_like_event(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(!is_modify_like_event(&EventKind::Any));
        assert!(!is_modify_like_event(&EventKind::Modify(
            ModifyKind::Metadata(MetadataKind::Any)
        )));
        assert!(!is_modify_like_event(&EventKind::Access(
            AccessKind::Close(AccessMode::Any)
        )));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_verbatim_paths_are_stripped_for_shell_apis() {
        assert_eq!(
            strip_windows_verbatim_prefix(PathBuf::from(r"\\?\C:\Users\Ada\AFS")),
            PathBuf::from(r"C:\Users\Ada\AFS")
        );
        assert_eq!(
            strip_windows_verbatim_prefix(PathBuf::from(r"\\?\UNC\server\share\AFS")),
            PathBuf::from(r"\\server\share\AFS")
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn root_relative_cloud_paths_use_sync_root_drive() {
        let context = ProviderContext {
            mount_id: "notion-main".to_string(),
            sync_root: PathBuf::from(r"C:\Users\Ada\AFS\Notion"),
            state_dir: PathBuf::from(r"C:\Users\Ada\AppData\Local\AFS"),
            identity_index: Default::default(),
        };

        assert_eq!(
            absolute_cloud_path(&context, Path::new(r"\Users\Ada\AFS\Notion\Page.md")),
            PathBuf::from(r"C:\Users\Ada\AFS\Notion\Page.md")
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn sync_root_membership_is_case_insensitive() {
        let context = ProviderContext {
            mount_id: "notion-main".to_string(),
            sync_root: PathBuf::from(r"C:\Users\Ada\AFS\Notion"),
            state_dir: PathBuf::from(r"C:\Users\Ada\AppData\Local\AFS"),
            identity_index: Default::default(),
        };

        assert!(path_is_under_sync_root(
            &context,
            Path::new(r"c:\users\ada\afs\notion\Draft.md")
        ));
        assert!(!path_is_under_sync_root(
            &context,
            Path::new(r"C:\Users\Ada\AFS\Notion Backup\Draft.md")
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn successful_placeholder_transfer_disables_on_demand_population() {
        use windows::Win32::Storage::CloudFilters::{
            CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
            CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_NONE,
        };

        assert_eq!(
            transfer_placeholders_flags_for_status(status_success()).0,
            CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION.0
        );
        assert_eq!(
            transfer_placeholders_flags_for_status(status_unsuccessful()).0,
            CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_NONE.0
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn local_create_retry_retries_until_success() {
        let attempts = std::cell::Cell::new(0);

        let value = retry_operation_until(
            std::time::Duration::from_secs(1),
            std::time::Duration::ZERO,
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() < 3 {
                    return Err(HelperError::new("transient", "not ready"));
                }
                Ok("ready")
            },
        )
        .expect("retry should eventually succeed");

        assert_eq!(value, "ready");
        assert_eq!(attempts.get(), 3);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn local_create_retry_returns_last_error_after_timeout() {
        let attempts = std::cell::Cell::new(0);

        let error = retry_operation_until(
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            || -> Result<(), HelperError> {
                attempts.set(attempts.get() + 1);
                Err(HelperError::new("transient", "still locked"))
            },
        )
        .expect_err("retry should return the operation error");

        assert_eq!(error.code, "transient");
        assert_eq!(error.message, "still locked");
        assert_eq!(attempts.get(), 1);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn stale_pending_page_directory_delete_is_idempotent() {
        let error = HelperError::new(
            "daemon_error",
            "invalid_state: invalid state: virtual filesystem item `children:local:123` is not present in daemon state",
        );

        assert!(stale_pending_page_directory_delete(
            "children:local:123",
            &error
        ));
        assert!(!stale_pending_page_directory_delete(
            "children:page-1",
            &error
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parent_identifier_can_use_pending_local_directory_cache() {
        let context = ProviderContext {
            mount_id: "notion-main".to_string(),
            sync_root: PathBuf::from(r"C:\Users\Ada\AFS\Notion"),
            state_dir: PathBuf::from(r"C:\Users\Ada\AppData\Local\AFS"),
            identity_index: Default::default(),
        };
        let directory = Path::new(r"C:\Users\Ada\AFS\Notion\Draft");
        context.remember_path_identity(directory, "children:local:123");

        assert_eq!(
            parent_identifier_for_path(&context, directory).expect("parent identifier"),
            "children:local:123"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn identity_for_path_can_use_provider_cache() {
        let context = ProviderContext {
            mount_id: "notion-main".to_string(),
            sync_root: PathBuf::from(r"C:\Users\Ada\AFS\Notion"),
            state_dir: PathBuf::from(r"C:\Users\Ada\AppData\Local\AFS"),
            identity_index: Default::default(),
        };
        let page = Path::new(r"C:\Users\Ada\AFS\Notion\Draft\page.md");
        context.remember_path_identity(page, "local:123");

        assert_eq!(
            identity_for_path(&context, page).expect("identity"),
            Some("local:123".to_string())
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn provider_identity_index_moves_renamed_subtrees() {
        let index = ProviderIdentityIndex::default();
        let source = Path::new(r"C:\Users\Ada\AFS\Notion\Draft");
        let child = source.join("page.md");
        let target = Path::new(r"C:\Users\Ada\AFS\Notion\Renamed");

        index.remember(source, "children:local:123");
        index.remember(&child, "local:123");
        index.move_subtree(source, target);

        assert_eq!(index.get(target).as_deref(), Some("children:local:123"));
        assert_eq!(
            index.get(&target.join("page.md")).as_deref(),
            Some("local:123")
        );
        assert_eq!(index.get(source), None);
        assert_eq!(index.get(&child), None);
    }
}
