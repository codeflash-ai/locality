use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command as ProcessCommand;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use afs_connector::ConnectorUndoApplier;
use afs_core::AfsError;
use afs_core::journal::PushId;
use afs_core::model::{MountId, RemoteId};
use afs_notion::oauth::{
    DEFAULT_AFS_NOTION_OAUTH_BROKER_URL, HttpNotionOAuthBrokerClient, HttpNotionOAuthClient,
    NotionOAuthBrokerStart,
};
use afs_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, ConnectorProfileRepository,
    JournalRepository, MountConfig, MountRepository, ProjectionMode, SqliteStateStore,
    open_credential_store,
};
use afsd::execution::PushJobReport;
use afsd::file_provider as daemon_file_provider;
use afsd::ipc::{DaemonClientError, DaemonRequest, send_request_with_timeout};
use afsd::virtual_fs::{VirtualFsChildrenReport, virtual_fs_ancestor_container_identifiers};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::connect::{
    BrokerOAuthConnectOptions, ConnectError, ConnectOptions, ConnectReport, ConnectionShowReport,
    ConnectionsReport, DisconnectReport, HttpNotionConnectionProbe, OAuthConnectOptions,
    ProfilesReport, run_connect_notion, run_connect_notion_broker_oauth, run_connect_notion_oauth,
    run_connection_show, run_connections, run_disconnect, run_profiles,
};
use crate::connector::{
    ConnectorResolveError, SourceDescriptor, resolve_source_for_mount_id, resolve_source_for_path,
    source_descriptor,
};
use crate::daemon::{DaemonControlError, DaemonControlReport, run_daemon_control};
use crate::diff::{DiffError, run_diff_with_state_root};
use crate::doctor::{DoctorOptions, doctor_exit_code, print_doctor_report, run_doctor};
use crate::file_provider as file_provider_helper;
use crate::history::{
    HistoryError, LogOptions, LogReport, UndoReport, run_log, run_undo_with_applier,
    undo_report_exit_code,
};
use crate::info::{InfoError, InfoOptions, InfoReport, run_info};
use crate::inspect::{InspectError, InspectOptions, InspectReport, run_inspect};
use crate::local_oauth::{
    LocalOAuthAuthorization, LocalOAuthError, local_redirect, notion_authorize_url, random_state,
    run_local_oauth_authorization,
};
use crate::mount::{MountError, MountOptions, MountReport, run_mount};
use crate::pull::{PullError, PullReport, run_pull_with_state_root};
use crate::push::{
    PushOptions, PushReport, push_report_exit_code, run_push_with_daemon_at_state_root,
    select_push_targets,
};
use crate::restore::{RestoreError, RestoreOptions, RestoreReport, run_restore};
use crate::search::{SearchError, SearchOptions, SearchReport, notion_id_from_url, run_search};
use crate::status::{StatusError, StatusOptions, StatusReport, StatusSyncState, run_status};
use crate::templates::{
    TemplateApplyOptions, TemplateApplyReport, TemplateListReport, TemplateNewOptions,
    TemplateNewReport, TemplatePackError, TemplateValidateReport, run_template_apply,
    run_template_list, run_template_new, run_template_validate,
};

const EXIT_SUCCESS: i32 = 0;
const EXIT_INTERNAL: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_VALIDATION: i32 = 3;
const DEFAULT_DAEMON_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_DAEMON_MUTATING_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Parser)]
#[command(
    name = "afs",
    about = "AgentFS command line interface",
    long_about = "AgentFS projects remote workspaces, such as Notion, as local Markdown files that can be inspected, edited, pulled, pushed, and reconciled.",
    disable_help_subcommand = true
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Emit machine-readable JSON for command output. Ignored when printing help."
    )]
    json: bool,

    #[command(subcommand)]
    command: Option<AfsCommand>,
}

#[derive(Debug, Subcommand)]
enum AfsCommand {
    #[command(about = "Connect AgentFS to a remote source")]
    Connect {
        #[command(subcommand)]
        command: ConnectCommand,
    },
    #[command(about = "List saved source connections")]
    Connections,
    #[command(about = "List connector profiles")]
    Profiles,
    #[command(about = "Inspect or manage a saved source connection")]
    Connection {
        #[command(subcommand)]
        command: ConnectionCommand,
    },
    #[command(about = "Disconnect and remove a saved source connection")]
    Disconnect(DisconnectArgs),
    #[command(about = "Start, stop, reload, or inspect the AgentFS daemon")]
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    #[command(about = "Mount a remote source into the local filesystem")]
    Mount {
        #[command(subcommand)]
        command: MountCommand,
    },
    #[command(about = "Show source, mount, and sync metadata for a path")]
    Info(PathArg),
    #[command(about = "Show local sync state for mounts or paths")]
    Status(PathArg),
    #[command(about = "Run read-only diagnostics for daemon, mounts, providers, and auth")]
    Doctor,
    #[command(about = "Search local mount metadata without contacting remote sources")]
    Search(SearchArgs),
    #[command(about = "List, validate, and create local template pack workspaces")]
    Templates {
        #[command(subcommand)]
        command: TemplatesCommand,
    },
    #[command(about = "Explain local and remote sync state for a path")]
    Inspect(PathArg),
    #[command(about = "Pull remote content into the local projection")]
    Pull(RequiredPathArg),
    #[command(about = "Push local changes back to the remote source")]
    Push(PushArgs),
    #[command(about = "Preview the push plan for local changes")]
    Diff(RequiredPathArg),
    #[command(about = "Undo a reconciled push using its journal entry")]
    Undo(UndoArgs),
    #[command(about = "List push journal entries")]
    Log(PathArg),
    #[command(about = "Restore a local file from the last synced shadow")]
    Restore(RestoreCliArgs),
    #[command(about = "Configuration commands")]
    Config,
    #[command(about = "Run the AFS MCP stdio server")]
    Mcp,
    #[command(
        name = "file-provider",
        about = "Manage virtual filesystem registration"
    )]
    FileProvider {
        #[command(subcommand)]
        command: FileProviderCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConnectCommand {
    #[command(about = "Connect a Notion workspace")]
    Notion(ConnectNotionArgs),
}

#[derive(Debug, Args)]
struct ConnectNotionArgs {
    #[arg(
        long,
        value_name = "ID",
        help = "Connection id to save. Defaults to notion-main."
    )]
    name: Option<String>,
    #[arg(long, help = "Read a Notion integration token from standard input.")]
    token_stdin: bool,
    #[arg(long, help = "Print the OAuth URL instead of opening a browser.")]
    no_browser: bool,
    #[arg(
        long,
        help = "Use direct Notion OAuth environment credentials instead of the broker."
    )]
    direct_oauth: bool,
    #[arg(long, value_name = "URL", help = "OAuth broker base URL.")]
    broker_url: Option<String>,
    #[arg(
        long,
        value_name = "URI",
        help = "OAuth redirect URI for the local callback listener."
    )]
    redirect_uri: Option<String>,
}

#[derive(Debug, Subcommand)]
enum ConnectionCommand {
    #[command(about = "Show connection details")]
    Show(ConnectionShowArgs),
}

#[derive(Debug, Args)]
struct ConnectionShowArgs {
    #[arg(value_name = "id", help = "Connection id to inspect.")]
    id: String,
}

#[derive(Debug, Args)]
struct DisconnectArgs {
    #[arg(value_name = "id", help = "Connection id to remove.")]
    id: String,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    #[command(about = "Start the daemon")]
    Start(DaemonArgs),
    #[command(about = "Stop the daemon")]
    Stop(DaemonArgs),
    #[command(about = "Show daemon status")]
    Status(DaemonArgs),
    #[command(about = "Reload daemon mount watches")]
    Reload(DaemonArgs),
    #[command(about = "Restart the daemon")]
    Restart(DaemonArgs),
}

#[derive(Debug, Args)]
struct DaemonArgs {
    #[arg(long, help = "Run afsd as a detached session process.")]
    session: bool,
    #[arg(long, help = "Run afsd with launchd. Supported on macOS only.")]
    launchd: bool,
    #[arg(long, value_name = "PATH", help = "Path to the afsd binary to launch.")]
    afsd_bin: Option<String>,
    #[arg(
        long,
        value_name = "PATH",
        help = "AgentFS state directory. Defaults to $AFS_STATE_DIR or ~/.afs."
    )]
    state_dir: Option<String>,
    #[arg(
        long,
        value_name = "host:port|off",
        help = "TCP listener address for daemon IPC, or off to disable."
    )]
    tcp_addr: Option<String>,
    #[arg(
        long,
        value_name = "KEY",
        help = "Environment variable to pass through to the daemon. Repeatable."
    )]
    include_env: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum MountCommand {
    #[command(about = "Mount Notion content")]
    Notion(MountNotionArgs),
}

#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("notion-root")
        .required(true)
        .args(["workspace", "root_page"])
))]
struct MountNotionArgs {
    #[arg(
        value_name = "path",
        help = "Local directory where the mount should be registered."
    )]
    path: String,
    #[arg(long, help = "Mount all Notion content shared with the integration.")]
    workspace: bool,
    #[arg(
        long,
        value_name = "page-id",
        help = "Mount a specific Notion root page."
    )]
    root_page: Option<String>,
    #[arg(long, value_name = "id", help = "Connection id to use for this mount.")]
    connection: Option<String>,
    #[arg(
        long,
        value_name = "id",
        help = "Mount id to save. Defaults to notion-main."
    )]
    mount_id: Option<String>,
    #[arg(
        long,
        value_name = "mode",
        help = "Projection mode. Supported values depend on the host platform."
    )]
    projection: Option<String>,
    #[arg(
        long,
        help = "Register the mount as read-only and block push operations."
    )]
    read_only: bool,
}

#[derive(Debug, Args)]
struct PathArg {
    #[arg(
        value_name = "path",
        help = "Path inside an AgentFS mount. Defaults to the current scope when omitted."
    )]
    path: Option<String>,
}

#[derive(Debug, Args)]
struct RequiredPathArg {
    #[arg(value_name = "path", help = "Path inside an AgentFS mount.")]
    path: String,
}

#[derive(Debug, Args)]
struct PushArgs {
    #[arg(value_name = "path", help = "File or directory scope to push.")]
    path: String,
    #[arg(
        short = 'y',
        long = "yes",
        help = "Approve safe push plans without prompting."
    )]
    yes: bool,
    #[arg(long, help = "Approve plans that trip destructive-change guardrails.")]
    confirm: bool,
}

#[derive(Debug, Args)]
struct UndoArgs {
    #[arg(value_name = "push-id", help = "Push journal id to undo.")]
    push_id: String,
}

#[derive(Debug, Args)]
struct RestoreCliArgs {
    #[arg(
        value_name = "path",
        help = "File path to restore from the last synced shadow."
    )]
    path: String,
    #[arg(long, help = "Restore even if the file is marked conflicted.")]
    force: bool,
}

#[derive(Debug, Args)]
struct SearchArgs {
    #[arg(
        value_name = "query",
        num_args = 1..,
        help = "Title, path fragment, remote id, or source URL to find locally."
    )]
    query: Vec<String>,
    #[arg(
        long,
        value_name = "connector",
        help = "Limit search to one connector."
    )]
    connector: Option<String>,
    #[arg(
        long,
        value_name = "n",
        default_value_t = 10,
        help = "Maximum results."
    )]
    limit: usize,
}

#[derive(Debug, Subcommand)]
enum TemplatesCommand {
    #[command(about = "List bundled template packs")]
    List,
    #[command(about = "Validate a template pack directory")]
    Validate(TemplateValidateArgs),
    #[command(about = "Create a local workspace from a template pack")]
    New(TemplateNewArgs),
    #[command(about = "Apply one template into a local or mounted folder")]
    Apply(TemplateApplyArgs),
}

#[derive(Debug, Args)]
struct TemplateValidateArgs {
    #[arg(
        value_name = "path",
        help = "Template pack directory or manifest path."
    )]
    path: String,
}

#[derive(Debug, Args)]
struct TemplateNewArgs {
    #[arg(value_name = "pack", help = "Bundled pack id or local pack path.")]
    pack: String,
    #[arg(value_name = "path", help = "Directory to create.")]
    path: String,
    #[arg(long, help = "Allow writing into a non-empty target directory.")]
    force: bool,
}

#[derive(Debug, Args)]
struct TemplateApplyArgs {
    #[arg(value_name = "pack", help = "Bundled pack id or local pack path.")]
    pack: String,
    #[arg(value_name = "template", help = "Template name, e.g. weekly-update.")]
    template: String,
    #[arg(
        long,
        value_name = "dir",
        help = "Directory to write the Markdown draft into."
    )]
    to: String,
    #[arg(
        long,
        value_name = "title",
        help = "Override frontmatter title and output filename."
    )]
    title: Option<String>,
    #[arg(long, help = "Overwrite an existing generated draft.")]
    force: bool,
}

#[derive(Debug, Subcommand)]
enum FileProviderCommand {
    #[command(about = "Register a virtual filesystem provider for a mount")]
    Register(FileProviderTargetArg),
    #[command(about = "Start the background provider runtime for a mount")]
    Start(FileProviderTargetArg),
    #[command(about = "Run the foreground Windows Cloud Files provider for a mount")]
    Run(FileProviderTargetArg),
    #[command(about = "Stop the background provider runtime for a mount")]
    Stop(FileProviderTargetArg),
    #[command(about = "Show provider runtime status for a mount")]
    Status(FileProviderTargetArg),
    #[command(about = "Restart the background provider runtime for a mount")]
    Restart(FileProviderTargetArg),
    #[command(about = "Open a registered virtual filesystem mount")]
    Open(FileProviderTargetArg),
    #[command(about = "Unregister a virtual filesystem provider for a mount")]
    Unregister(FileProviderTargetArg),
    #[command(about = "List registered file provider domains")]
    List,
    #[command(about = "Reset file provider registration state")]
    Reset,
}

#[derive(Debug, Args)]
struct FileProviderTargetArg {
    #[arg(
        value_name = "mount-id-or-path",
        help = "Mount id or path inside an AgentFS mount."
    )]
    target: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SpinnerConfig {
    enabled: bool,
    label: String,
}

fn spinner_enabled(json: bool, stderr_is_terminal: bool) -> bool {
    !json && stderr_is_terminal
}

fn spinner_config_for_command(
    command: &str,
    path: &str,
    json: bool,
    stderr_is_terminal: bool,
) -> SpinnerConfig {
    let verb = match command {
        "pull" => "pulling",
        "push" => "pushing",
        other => other,
    };
    SpinnerConfig {
        enabled: spinner_enabled(json, stderr_is_terminal),
        label: format!("{verb} {path}"),
    }
}

fn with_terminal_spinner<T>(config: SpinnerConfig, operation: impl FnOnce() -> T) -> T {
    let _spinner = TerminalSpinner::start(config);
    operation()
}

struct TerminalSpinner {
    stop: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl TerminalSpinner {
    fn start(config: SpinnerConfig) -> Option<Self> {
        if !config.enabled {
            return None;
        }

        let (stop, stop_rx) = mpsc::channel();
        let label = config.label;
        let handle = thread::spawn(move || {
            let frames = ["-", "\\", "|", "/"];
            let mut index = 0;
            loop {
                let mut stderr = io::stderr().lock();
                let _ = write!(stderr, "\r{} {}", frames[index % frames.len()], label);
                let _ = stderr.flush();
                drop(stderr);
                index += 1;
                match stop_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        });

        Some(Self {
            stop: Some(stop),
            handle: Some(handle),
        })
    }
}

impl Drop for TerminalSpinner {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let mut stderr = io::stderr().lock();
        let _ = write!(stderr, "\r\x1b[2K");
        let _ = stderr.flush();
    }
}

pub fn dispatch(args: &[String]) -> i32 {
    let cli = match parse_cli(args) {
        Ok(cli) => cli,
        Err(error) => {
            let exit_code = error.exit_code();
            let _ = error.print();
            return exit_code;
        }
    };

    let Some(command) = cli.command else {
        print_help();
        return EXIT_SUCCESS;
    };

    let legacy_args = legacy_args_for_command(&command);
    let json = cli.json;
    match command {
        AfsCommand::Connect { .. } => connect(&legacy_args[1..], json),
        AfsCommand::Connections => connections(&legacy_args[1..], json),
        AfsCommand::Profiles => profiles(&legacy_args[1..], json),
        AfsCommand::Connection { .. } => connection(&legacy_args[1..], json),
        AfsCommand::Disconnect(_) => disconnect(&legacy_args[1..], json),
        AfsCommand::Daemon { .. } => daemon(&legacy_args[1..], json),
        AfsCommand::Mount { .. } => mount(&legacy_args[1..], json),
        AfsCommand::Info(_) => info(&legacy_args[1..], json),
        AfsCommand::Status(_) => status(&legacy_args[1..], json),
        AfsCommand::Doctor => doctor(json),
        AfsCommand::Search(_) => search(&legacy_args[1..], json),
        AfsCommand::Templates { .. } => templates(&legacy_args[1..], json),
        AfsCommand::Inspect(_) => inspect(&legacy_args[1..], json),
        AfsCommand::Pull(_) => pull(&legacy_args[1..], json),
        AfsCommand::Push(_) => push(&legacy_args[1..], json),
        AfsCommand::Diff(_) => diff(&legacy_args[1..], json),
        AfsCommand::Restore(_) => restore(&legacy_args[1..], json),
        AfsCommand::Undo(_) => undo(&legacy_args[1..], json),
        AfsCommand::Log(_) => log(&legacy_args[1..], json),
        AfsCommand::Config => stub("config", json),
        AfsCommand::Mcp => mcp(),
        AfsCommand::FileProvider { .. } => file_provider(&legacy_args[1..], json),
    }
}

fn parse_cli(args: &[String]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(
        std::iter::once("afs".to_string())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>(),
    )
}

fn legacy_args_for_command(command: &AfsCommand) -> Vec<String> {
    let mut args = Vec::new();
    match command {
        AfsCommand::Connect { command } => {
            args.push("connect".to_string());
            match command {
                ConnectCommand::Notion(options) => {
                    args.push("notion".to_string());
                    push_optional_flag_value(&mut args, "--name", options.name.as_deref());
                    push_flag(&mut args, "--token-stdin", options.token_stdin);
                    push_flag(&mut args, "--no-browser", options.no_browser);
                    push_flag(&mut args, "--direct-oauth", options.direct_oauth);
                    push_optional_flag_value(
                        &mut args,
                        "--broker-url",
                        options.broker_url.as_deref(),
                    );
                    push_optional_flag_value(
                        &mut args,
                        "--redirect-uri",
                        options.redirect_uri.as_deref(),
                    );
                }
            }
        }
        AfsCommand::Connections => args.push("connections".to_string()),
        AfsCommand::Profiles => args.push("profiles".to_string()),
        AfsCommand::Connection { command } => {
            args.push("connection".to_string());
            match command {
                ConnectionCommand::Show(options) => {
                    args.push("show".to_string());
                    args.push(options.id.clone());
                }
            }
        }
        AfsCommand::Disconnect(options) => {
            args.push("disconnect".to_string());
            args.push(options.id.clone());
        }
        AfsCommand::Daemon { command } => {
            args.push("daemon".to_string());
            match command {
                DaemonCommand::Start(options) => {
                    args.push("start".to_string());
                    push_daemon_args(&mut args, options);
                }
                DaemonCommand::Stop(options) => {
                    args.push("stop".to_string());
                    push_daemon_args(&mut args, options);
                }
                DaemonCommand::Status(options) => {
                    args.push("status".to_string());
                    push_daemon_args(&mut args, options);
                }
                DaemonCommand::Reload(options) => {
                    args.push("reload".to_string());
                    push_daemon_args(&mut args, options);
                }
                DaemonCommand::Restart(options) => {
                    args.push("restart".to_string());
                    push_daemon_args(&mut args, options);
                }
            }
        }
        AfsCommand::Mount { command } => {
            args.push("mount".to_string());
            match command {
                MountCommand::Notion(options) => {
                    args.push("notion".to_string());
                    args.push(options.path.clone());
                    push_flag(&mut args, "--workspace", options.workspace);
                    push_optional_flag_value(
                        &mut args,
                        "--root-page",
                        options.root_page.as_deref(),
                    );
                    push_optional_flag_value(
                        &mut args,
                        "--connection",
                        options.connection.as_deref(),
                    );
                    push_optional_flag_value(&mut args, "--mount-id", options.mount_id.as_deref());
                    push_optional_flag_value(
                        &mut args,
                        "--projection",
                        options.projection.as_deref(),
                    );
                    push_flag(&mut args, "--read-only", options.read_only);
                }
            }
        }
        AfsCommand::Info(options) => {
            args.push("info".to_string());
            push_optional_positional(&mut args, options.path.as_deref());
        }
        AfsCommand::Status(options) => {
            args.push("status".to_string());
            push_optional_positional(&mut args, options.path.as_deref());
        }
        AfsCommand::Doctor => args.push("doctor".to_string()),
        AfsCommand::Search(options) => {
            args.push("search".to_string());
            for query_part in &options.query {
                args.push(query_part.clone());
            }
            push_optional_flag_value(&mut args, "--connector", options.connector.as_deref());
            push_flag_value(&mut args, "--limit", &options.limit.to_string());
        }
        AfsCommand::Templates { command } => {
            args.push("templates".to_string());
            match command {
                TemplatesCommand::List => args.push("list".to_string()),
                TemplatesCommand::Validate(options) => {
                    args.push("validate".to_string());
                    args.push(options.path.clone());
                }
                TemplatesCommand::New(options) => {
                    args.push("new".to_string());
                    args.push(options.pack.clone());
                    args.push(options.path.clone());
                    push_flag(&mut args, "--force", options.force);
                }
                TemplatesCommand::Apply(options) => {
                    args.push("apply".to_string());
                    args.push(options.pack.clone());
                    args.push(options.template.clone());
                    push_flag_value(&mut args, "--to", &options.to);
                    push_optional_flag_value(&mut args, "--title", options.title.as_deref());
                    push_flag(&mut args, "--force", options.force);
                }
            }
        }
        AfsCommand::Inspect(options) => {
            args.push("inspect".to_string());
            push_optional_positional(&mut args, options.path.as_deref());
        }
        AfsCommand::Pull(options) => {
            args.push("pull".to_string());
            args.push(options.path.clone());
        }
        AfsCommand::Push(options) => {
            args.push("push".to_string());
            args.push(options.path.clone());
            push_flag(&mut args, "--yes", options.yes);
            push_flag(&mut args, "--confirm", options.confirm);
        }
        AfsCommand::Diff(options) => {
            args.push("diff".to_string());
            args.push(options.path.clone());
        }
        AfsCommand::Undo(options) => {
            args.push("undo".to_string());
            args.push(options.push_id.clone());
        }
        AfsCommand::Log(options) => {
            args.push("log".to_string());
            push_optional_positional(&mut args, options.path.as_deref());
        }
        AfsCommand::Restore(options) => {
            args.push("restore".to_string());
            args.push(options.path.clone());
            push_flag(&mut args, "--force", options.force);
        }
        AfsCommand::Config => args.push("config".to_string()),
        AfsCommand::Mcp => args.push("mcp".to_string()),
        AfsCommand::FileProvider { command } => {
            args.push("file-provider".to_string());
            match command {
                FileProviderCommand::Register(options) => {
                    args.push("register".to_string());
                    args.push(options.target.clone());
                }
                FileProviderCommand::Start(options) => {
                    args.push("start".to_string());
                    args.push(options.target.clone());
                }
                FileProviderCommand::Run(options) => {
                    args.push("run".to_string());
                    args.push(options.target.clone());
                }
                FileProviderCommand::Stop(options) => {
                    args.push("stop".to_string());
                    args.push(options.target.clone());
                }
                FileProviderCommand::Status(options) => {
                    args.push("status".to_string());
                    args.push(options.target.clone());
                }
                FileProviderCommand::Restart(options) => {
                    args.push("restart".to_string());
                    args.push(options.target.clone());
                }
                FileProviderCommand::Open(options) => {
                    args.push("open".to_string());
                    args.push(options.target.clone());
                }
                FileProviderCommand::Unregister(options) => {
                    args.push("unregister".to_string());
                    args.push(options.target.clone());
                }
                FileProviderCommand::List => args.push("list".to_string()),
                FileProviderCommand::Reset => args.push("reset".to_string()),
            }
        }
    }
    args
}

fn mcp() -> i32 {
    let config = match afsd::mcp::McpServerConfig::discover(&default_state_root()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("afs mcp: {error}");
            return EXIT_INTERNAL;
        }
    };
    match afsd::mcp::serve_stdio(config) {
        Ok(()) => EXIT_SUCCESS,
        Err(error) => {
            eprintln!("afs mcp: {error}");
            EXIT_INTERNAL
        }
    }
}

fn push_daemon_args(args: &mut Vec<String>, options: &DaemonArgs) {
    push_flag(args, "--session", options.session);
    push_flag(args, "--launchd", options.launchd);
    push_optional_flag_value(args, "--afsd-bin", options.afsd_bin.as_deref());
    push_optional_flag_value(args, "--state-dir", options.state_dir.as_deref());
    push_optional_flag_value(args, "--tcp-addr", options.tcp_addr.as_deref());
    for value in &options.include_env {
        push_flag_value(args, "--include-env", value);
    }
}

fn push_flag(args: &mut Vec<String>, flag: &str, enabled: bool) {
    if enabled {
        args.push(flag.to_string());
    }
}

fn push_optional_positional(args: &mut Vec<String>, value: Option<&str>) {
    if let Some(value) = value {
        args.push(value.to_string());
    }
}

fn push_optional_flag_value(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value {
        push_flag_value(args, flag, value);
    }
}

fn push_flag_value(args: &mut Vec<String>, flag: &str, value: &str) {
    args.push(flag.to_string());
    args.push(value.to_string());
}

fn daemon(args: &[String], json: bool) -> i32 {
    match run_daemon_control(args) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_daemon_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => daemon_command_error(json, error),
    }
}

fn doctor(json: bool) -> i32 {
    let report = run_doctor(DoctorOptions::default());
    let exit_code = doctor_exit_code(&report);
    if json {
        print_json(&report);
    } else {
        print_doctor_report(&report);
    }
    exit_code
}

fn connect(args: &[String], json: bool) -> i32 {
    if first_positional(args) != Some("notion") {
        return command_error(
            json,
            CommandError::new(
                "connect",
                "usage",
                "usage: afs connect notion [--name <id>] [--token-stdin|--no-browser|--direct-oauth] [--broker-url <url>] [--redirect-uri <uri>] [--json]",
            ),
            EXIT_USAGE,
        );
    }

    let state_root = default_state_root();
    let mut store = match SqliteStateStore::open(state_root.clone()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("connect", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let credentials = open_credential_store(&state_root);

    if has_flag(args, "--token-stdin") {
        let token = match read_connect_token(args, json) {
            Ok(token) => token,
            Err(error) => return command_error(json, error, EXIT_INTERNAL),
        };
        if token.is_empty() {
            return command_error(
                json,
                CommandError::new("connect", "auth_required", "empty Notion token")
                    .with_suggested_command("afs connect notion --token-stdin"),
                EXIT_INTERNAL,
            );
        }

        let options = ConnectOptions {
            connection_id: flag_value(args, "--name").map(ConnectionId::new),
            token,
        };
        let probe = HttpNotionConnectionProbe;
        return match run_connect_notion(&mut store, credentials.as_ref(), options, &probe) {
            Ok(report) if json => {
                print_json(&report);
                EXIT_SUCCESS
            }
            Ok(report) => {
                print_connect_report(&report);
                EXIT_SUCCESS
            }
            Err(error) => connect_command_error("connect", json, error),
        };
    }

    if !has_flag(args, "--direct-oauth") {
        let broker_config = match notion_oauth_broker_config(args) {
            Ok(config) => config,
            Err(error) => return command_error(json, error, EXIT_INTERNAL),
        };
        let broker = HttpNotionOAuthBrokerClient::new(broker_config.broker_url.clone());
        let start = match broker.start(&NotionOAuthBrokerStart {
            redirect_uri: broker_config.redirect_uri,
        }) {
            Ok(start) => start,
            Err(error) => {
                return command_error(
                    json,
                    CommandError::new(
                        "connect",
                        "oauth_broker_start_failed",
                        format!("Notion OAuth broker start failed: {error}"),
                    )
                    .with_suggested_command("afs connect notion --token-stdin"),
                    EXIT_INTERNAL,
                );
            }
        };
        let authorization_url = start.normalized_authorization_url();
        let authorization = match run_local_oauth_authorization(
            "Notion",
            &authorization_url,
            &start.redirect_uri,
            &start.state,
            has_flag(args, "--no-browser"),
            json,
        ) {
            Ok(authorization) => authorization,
            Err(error) => {
                return command_error(json, local_oauth_command_error(error), EXIT_INTERNAL);
            }
        };
        let options = BrokerOAuthConnectOptions {
            connection_id: flag_value(args, "--name").map(ConnectionId::new),
            broker_url: broker_config.broker_url,
            client_id: start.client_id,
            session: start.session,
            state: start.state,
            code: authorization.code,
            redirect_uri: start.redirect_uri,
        };
        return match run_connect_notion_broker_oauth(
            &mut store,
            credentials.as_ref(),
            options,
            &broker,
        ) {
            Ok(report) if json => {
                print_json(&report);
                EXIT_SUCCESS
            }
            Ok(report) => {
                print_connect_report(&report);
                EXIT_SUCCESS
            }
            Err(error) => connect_command_error("connect", json, error),
        };
    }

    let oauth_config = match notion_oauth_config(args) {
        Ok(config) => config,
        Err(error) => return command_error(json, error, EXIT_INTERNAL),
    };
    let authorization =
        match run_local_notion_oauth(&oauth_config, has_flag(args, "--no-browser"), json) {
            Ok(authorization) => authorization,
            Err(error) => return command_error(json, error, EXIT_INTERNAL),
        };
    let options = OAuthConnectOptions {
        connection_id: flag_value(args, "--name").map(ConnectionId::new),
        client_id: oauth_config.client_id,
        client_secret: oauth_config.client_secret,
        code: authorization.code,
        redirect_uri: oauth_config.redirect_uri,
    };
    let exchange = HttpNotionOAuthClient::new();
    match run_connect_notion_oauth(&mut store, credentials.as_ref(), options, &exchange) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_connect_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => connect_command_error("connect", json, error),
    }
}

fn connections(args: &[String], json: bool) -> i32 {
    if first_positional(args).is_some() {
        return command_error(
            json,
            CommandError::new("connections", "usage", "usage: afs connections [--json]"),
            EXIT_USAGE,
        );
    }

    let store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("connections", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };

    match run_connections(&store) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_connections_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => connect_command_error("connections", json, error),
    }
}

fn profiles(args: &[String], json: bool) -> i32 {
    if first_positional(args).is_some() {
        return command_error(
            json,
            CommandError::new("profiles", "usage", "usage: afs profiles [--json]"),
            EXIT_USAGE,
        );
    }

    let store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("profiles", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };

    match run_profiles(&store) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_profiles_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => connect_command_error("profiles", json, error),
    }
}

fn connection(args: &[String], json: bool) -> i32 {
    if first_positional(args) != Some("show") {
        return command_error(
            json,
            CommandError::new(
                "connection",
                "usage",
                "usage: afs connection show <id> [--json]",
            ),
            EXIT_USAGE,
        );
    }
    let Some(connection_id) = nth_positional(args, 1) else {
        return command_error(
            json,
            CommandError::new(
                "connection",
                "usage",
                "usage: afs connection show <id> [--json]",
            ),
            EXIT_USAGE,
        );
    };

    let store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("connection", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };

    match run_connection_show(&store, ConnectionId::new(connection_id)) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_connection_show_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => connect_command_error("connection", json, error),
    }
}

fn disconnect(args: &[String], json: bool) -> i32 {
    let Some(connection_id) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new("disconnect", "usage", "usage: afs disconnect <id> [--json]"),
            EXIT_USAGE,
        );
    };

    let state_root = default_state_root();
    let mut store = match SqliteStateStore::open(state_root.clone()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("disconnect", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let credentials = open_credential_store(&state_root);

    match run_disconnect(
        &mut store,
        credentials.as_ref(),
        ConnectionId::new(connection_id),
    ) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_disconnect_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => connect_command_error("disconnect", json, error),
    }
}

fn file_provider(args: &[String], json: bool) -> i32 {
    let Some(action) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new(
                "file-provider",
                "usage",
                "usage: afs file-provider register|start|run|stop|status|restart|open|unregister <mount-id-or-path> [--json]",
            ),
            EXIT_USAGE,
        );
    };

    match action {
        "register" => file_provider_register(args, json),
        "start" => file_provider_lifecycle(
            args,
            json,
            file_provider_helper::WindowsCloudFilesLifecycleAction::Start,
        ),
        "run" => file_provider_run(args, json),
        "stop" => file_provider_lifecycle(
            args,
            json,
            file_provider_helper::WindowsCloudFilesLifecycleAction::Stop,
        ),
        "status" => file_provider_lifecycle(
            args,
            json,
            file_provider_helper::WindowsCloudFilesLifecycleAction::Status,
        ),
        "restart" => file_provider_lifecycle(
            args,
            json,
            file_provider_helper::WindowsCloudFilesLifecycleAction::Restart,
        ),
        "open" => file_provider_open(args, json),
        "unregister" => file_provider_unregister(args, json),
        "list" => run_platform_file_provider_helper(
            json,
            "list",
            windows_cloud_files_state_args_for_platform(),
            None,
        ),
        "reset" => run_platform_file_provider_helper(
            json,
            "reset",
            windows_cloud_files_state_args_for_platform(),
            None,
        ),
        _ => command_error(
            json,
            CommandError::new(
                "file-provider",
                "usage",
                "usage: afs file-provider register|start|run|stop|status|restart|open|unregister|list|reset",
            ),
            EXIT_USAGE,
        ),
    }
}

fn file_provider_register(args: &[String], json: bool) -> i32 {
    let Some(target) = nth_positional(args, 1) else {
        return command_error(
            json,
            CommandError::new(
                "file-provider",
                "usage",
                "usage: afs file-provider register <mount-id-or-path> [--json]",
            ),
            EXIT_USAGE,
        );
    };

    let store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("file-provider", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let mount = match resolve_mount_target(&store, target) {
        Ok(mount) => mount,
        Err(message) => {
            return command_error(
                json,
                CommandError::new("file-provider", "mount_not_found", message),
                EXIT_USAGE,
            );
        }
    };
    let target_os = std::env::consts::OS;
    let registration = match validate_virtual_projection_registration(&mount, target_os) {
        Ok(registration) => registration,
        Err(error) => return command_error(json, error, EXIT_USAGE),
    };

    let mount_id = mount.mount_id.0.clone();
    match registration {
        VirtualProjectionRegistration::MacosFileProvider => {
            let display_name = file_provider_display_name(&mount);
            run_file_provider_helper(
                json,
                "register",
                vec![
                    "--mount-id".to_string(),
                    mount_id.clone(),
                    "--display-name".to_string(),
                    display_name,
                ],
                Some(mount_id),
            )
        }
        VirtualProjectionRegistration::LinuxFuse => run_linux_fuse_register(json, &mount),
        VirtualProjectionRegistration::WindowsCloudFiles => {
            run_windows_cloud_files_register(json, &mount)
        }
    }
}

fn file_provider_lifecycle(
    args: &[String],
    json: bool,
    action: file_provider_helper::WindowsCloudFilesLifecycleAction,
) -> i32 {
    let Some(target) = nth_positional(args, 1) else {
        return command_error(
            json,
            CommandError::new(
                "file-provider",
                "usage",
                format!(
                    "usage: afs file-provider {} <mount-id-or-path> [--json]",
                    action.as_str()
                ),
            ),
            EXIT_USAGE,
        );
    };

    let store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("file-provider", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let mount = match resolve_mount_target(&store, target) {
        Ok(mount) => mount,
        Err(message) => {
            return command_error(
                json,
                CommandError::new("file-provider", "mount_not_found", message),
                EXIT_USAGE,
            );
        }
    };
    let registration = match validate_virtual_projection_registration(&mount, std::env::consts::OS)
    {
        Ok(registration) => registration,
        Err(error) => return command_error(json, error, EXIT_USAGE),
    };

    match registration {
        VirtualProjectionRegistration::WindowsCloudFiles => {
            run_windows_cloud_files_lifecycle(json, &mount, action)
        }
        VirtualProjectionRegistration::MacosFileProvider
        | VirtualProjectionRegistration::LinuxFuse => command_error(
            json,
            CommandError::new(
                "file-provider",
                "unsupported_platform",
                format!(
                    "file-provider {} is currently implemented for Windows Cloud Files mounts",
                    action.as_str()
                ),
            ),
            EXIT_USAGE,
        ),
    }
}

fn file_provider_run(args: &[String], json: bool) -> i32 {
    let Some(target) = nth_positional(args, 1) else {
        return command_error(
            json,
            CommandError::new(
                "file-provider",
                "usage",
                "usage: afs file-provider run <mount-id-or-path> [--json]",
            ),
            EXIT_USAGE,
        );
    };

    let store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("file-provider", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let mount = match resolve_mount_target(&store, target) {
        Ok(mount) => mount,
        Err(message) => {
            return command_error(
                json,
                CommandError::new("file-provider", "mount_not_found", message),
                EXIT_USAGE,
            );
        }
    };
    let target_os = std::env::consts::OS;
    let registration = match validate_virtual_projection_registration(&mount, target_os) {
        Ok(registration) => registration,
        Err(error) => return command_error(json, error, EXIT_USAGE),
    };

    match registration {
        VirtualProjectionRegistration::WindowsCloudFiles => {
            run_windows_cloud_files_run(json, &mount)
        }
        VirtualProjectionRegistration::MacosFileProvider
        | VirtualProjectionRegistration::LinuxFuse => command_error(
            json,
            CommandError::new(
                "file-provider",
                "unsupported_platform",
                "foreground provider run is only supported for Windows Cloud Files",
            ),
            EXIT_USAGE,
        ),
    }
}

fn file_provider_open(args: &[String], json: bool) -> i32 {
    let Some(target) = nth_positional(args, 1) else {
        return command_error(
            json,
            CommandError::new(
                "file-provider",
                "usage",
                "usage: afs file-provider open <mount-id-or-path> [--json]",
            ),
            EXIT_USAGE,
        );
    };

    let store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("file-provider", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let mount = match resolve_mount_target(&store, target) {
        Ok(mount) => mount,
        Err(message) => {
            return command_error(
                json,
                CommandError::new("file-provider", "mount_not_found", message),
                EXIT_USAGE,
            );
        }
    };
    let target_os = std::env::consts::OS;
    let registration = match validate_virtual_projection_registration(&mount, target_os) {
        Ok(registration) => registration,
        Err(error) => return command_error(json, error, EXIT_USAGE),
    };

    match registration {
        VirtualProjectionRegistration::MacosFileProvider => run_file_provider_helper(
            json,
            "open",
            vec!["--mount-id".to_string(), mount.mount_id.0.clone()],
            Some(mount.mount_id.0),
        ),
        VirtualProjectionRegistration::LinuxFuse => open_path_for_linux_fuse(json, &mount),
        VirtualProjectionRegistration::WindowsCloudFiles => {
            run_windows_cloud_files_open(json, &mount)
        }
    }
}

fn file_provider_unregister(args: &[String], json: bool) -> i32 {
    let Some(target) = nth_positional(args, 1) else {
        return command_error(
            json,
            CommandError::new(
                "file-provider",
                "usage",
                "usage: afs file-provider unregister <mount-id-or-path> [--json]",
            ),
            EXIT_USAGE,
        );
    };

    let target_os = std::env::consts::OS;
    let resolved_mount = SqliteStateStore::open(default_state_root())
        .ok()
        .and_then(|store| resolve_mount_target(&store, target).ok());
    if target_os == "linux" {
        return run_linux_fuse_unregister(json, resolved_mount.as_ref(), target);
    }
    if target_os == "windows" {
        let mount_id = resolved_mount
            .map(|mount| mount.mount_id.0)
            .unwrap_or_else(|| target.to_string());
        return run_windows_cloud_files_unregister(json, &mount_id);
    }

    let mount_id = match resolved_mount {
        Some(mount) => mount.mount_id.0,
        None => target.to_string(),
    };
    run_file_provider_helper(
        json,
        "unregister",
        vec!["--mount-id".to_string(), mount_id.clone()],
        Some(mount_id),
    )
}

fn restore(args: &[String], json: bool) -> i32 {
    let Some(path) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new(
                "restore",
                "usage",
                "usage: afs restore <path> [--force] [--json]",
            ),
            EXIT_USAGE,
        );
    };

    let state_root = default_state_root();
    let mut store = match SqliteStateStore::open(state_root.clone()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("restore", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let options = RestoreOptions {
        force: has_flag(args, "--force"),
        state_root: Some(state_root),
    };

    match run_restore(&mut store, PathBuf::from(path), options) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_restore_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => restore_command_error(json, error),
    }
}

fn mount(args: &[String], json: bool) -> i32 {
    let descriptor = source_descriptor("notion");
    if first_positional(args) != Some(descriptor.id()) {
        return command_error(
            json,
            CommandError::new("mount", "usage", mount_usage()),
            EXIT_USAGE,
        );
    }

    let Some(root) = nth_positional(args, 1) else {
        return command_error(
            json,
            CommandError::new("mount", "usage", mount_usage()),
            EXIT_USAGE,
        );
    };
    let root_page_id = flag_value(args, "--root-page");
    let workspace_mount = has_flag(args, "--workspace");
    if root_page_id.is_some() && workspace_mount {
        return command_error(
            json,
            CommandError::new(
                "mount",
                "usage",
                format!(
                    "afs mount {} accepts either --workspace or --root-page <page-id>, not both",
                    descriptor.id()
                ),
            ),
            EXIT_USAGE,
        );
    }
    if root_page_id.is_none() && !workspace_mount {
        return command_error(
            json,
            CommandError::new(
                "mount",
                "usage",
                format!(
                    "afs mount {} requires --workspace or --root-page <page-id>",
                    descriptor.id()
                ),
            ),
            EXIT_USAGE,
        );
    }

    let projection = match projection_mode(args) {
        Ok(projection) => projection,
        Err(message) => {
            return command_error(
                json,
                CommandError::new("mount", "usage", message),
                EXIT_USAGE,
            );
        }
    };

    let state_root = default_state_root();
    let mut store = match SqliteStateStore::open(state_root.clone()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("mount", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let connection_id = match resolve_mount_connection(&store, args) {
        Ok(connection_id) => connection_id,
        Err(error) => return command_error(json, error, EXIT_INTERNAL),
    };

    let options = MountOptions {
        mount_id: MountId::new(
            flag_value(args, "--mount-id")
                .map(str::to_string)
                .unwrap_or_else(|| descriptor.default_mount_id().to_string()),
        ),
        connector: descriptor.id().to_string(),
        root: PathBuf::from(root),
        remote_root_id: root_page_id.map(RemoteId::new),
        connection_id,
        read_only: has_flag(args, "--read-only"),
        projection,
    };
    let mount_id = options.mount_id.clone();

    match run_mount(&mut store, options) {
        Ok(report) => {
            notify_daemon_mounts_changed(&state_root);
            if let Err(error) = auto_register_mounted_projection(&state_root, &store, &mount_id) {
                return command_error(json, error, EXIT_INTERNAL);
            }
            if json {
                print_json(&report);
            } else {
                print_mount_report(&report);
            }
            EXIT_SUCCESS
        }
        Err(error) => mount_command_error(json, error),
    }
}

fn pull(args: &[String], json: bool) -> i32 {
    let Some(path) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new("pull", "usage", "usage: afs pull <path> [--json]"),
            EXIT_USAGE,
        );
    };

    let state_root = default_state_root();
    let stderr_is_terminal = io::stderr().is_terminal();
    let spinner_config = spinner_config_for_command("pull", path, json, stderr_is_terminal);
    let daemon_report = with_terminal_spinner(spinner_config.clone(), || {
        run_daemon_report::<PullReport>(
            &state_root,
            &DaemonRequest::Pull {
                path: PathBuf::from(path),
            },
        )
    });
    let fallback_reason = match daemon_report {
        DaemonReport::Report(report) if json => {
            let exit_code = pull_report_exit_code(&report);
            print_json(&report);
            return exit_code;
        }
        DaemonReport::Report(report) => {
            let exit_code = pull_report_exit_code(&report);
            print_pull_report(&report);
            return exit_code;
        }
        DaemonReport::Unavailable(reason) => reason,
        DaemonReport::Error(error) => {
            return command_error(
                json,
                CommandError::new("pull", error.code, error.message),
                error.exit_code,
            );
        }
    };
    if let Some(error) = pull_direct_fallback_error(fallback_reason, None) {
        return command_error(json, error, EXIT_INTERNAL);
    }

    let mut store = match SqliteStateStore::open(state_root.clone()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("pull", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let fallback_mount = resolve_mount_target(&store, path).ok();
    if let Some(error) = pull_direct_fallback_error(fallback_reason, fallback_mount.as_ref()) {
        return command_error(json, error, EXIT_INTERNAL);
    }
    warn_daemon_fallback("pull", fallback_reason);

    let credentials = open_credential_store(&state_root);
    let connector = match resolve_source_for_path(&store, credentials.as_ref(), path) {
        Ok(connector) => connector,
        Err(error) => return connector_command_error("pull", json, error),
    };

    match with_terminal_spinner(spinner_config, || {
        run_pull_with_state_root(
            &mut store,
            &connector,
            PathBuf::from(path),
            Some(&state_root),
        )
    }) {
        Ok(report) if json => {
            let exit_code = pull_report_exit_code(&report);
            print_json(&report);
            exit_code
        }
        Ok(report) => {
            let exit_code = pull_report_exit_code(&report);
            print_pull_report(&report);
            exit_code
        }
        Err(error) => pull_command_error(json, error),
    }
}

fn status(args: &[String], json: bool) -> i32 {
    let state_root = default_state_root();
    let mut store = match SqliteStateStore::open(state_root.clone()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("status", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let options = StatusOptions {
        path: first_positional(args).map(PathBuf::from),
        state_root: Some(state_root.clone()),
    };
    reconcile_projection_changes_best_effort(
        "status",
        &mut store,
        &state_root,
        options.path.as_deref(),
    );

    match run_status(&store, options) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_status_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => status_command_error(json, error, state_root),
    }
}

fn search(args: &[String], json: bool) -> i32 {
    let query = positional_args(args).join(" ");
    let limit = match flag_value(args, "--limit") {
        Some(value) => match value.parse::<usize>() {
            Ok(limit) => limit,
            Err(_) => {
                return command_error(
                    json,
                    CommandError::new(
                        "search",
                        "invalid_limit",
                        "--limit must be a positive integer",
                    ),
                    EXIT_USAGE,
                );
            }
        },
        None => 10,
    };
    let state_root = default_state_root();
    let mut store = match SqliteStateStore::open(state_root.clone()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("search", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let options = SearchOptions {
        query,
        connector: flag_value(args, "--connector").map(str::to_string),
        limit,
    };

    let report = match run_search(&store, options.clone()) {
        Ok(report) => report,
        Err(error) => return search_command_error(json, error),
    };
    let report = match refresh_notion_url_search_on_miss(&state_root, &mut store, &options, report)
    {
        Ok(report) => report,
        Err(error) => return command_error(json, error, EXIT_INTERNAL),
    };
    if let Err(error) = prefetch_notion_url_search_result_ancestors(&state_root, &options, &report)
    {
        return command_error(json, error, EXIT_INTERNAL);
    }

    if json {
        print_json(&report);
        EXIT_SUCCESS
    } else {
        print_search_report(&report);
        EXIT_SUCCESS
    }
}

fn refresh_notion_url_search_on_miss(
    state_root: &Path,
    store: &mut SqliteStateStore,
    options: &SearchOptions,
    report: SearchReport,
) -> Result<SearchReport, CommandError> {
    if !should_refresh_notion_url_search(options, &report) {
        return Ok(report);
    }

    let mounts = store
        .load_mounts()
        .map_err(|error| CommandError::new("search", "store_error", error.to_string()))?
        .into_iter()
        .filter(|mount| mount.connector == "notion")
        .collect::<Vec<_>>();
    if mounts.is_empty() {
        return Ok(report);
    }

    let mut refreshed = false;
    let mut errors = Vec::new();
    for mount in &mounts {
        match refresh_search_mount_metadata(state_root, store, mount) {
            Ok(()) => refreshed = true,
            Err(error) => errors.push(error.message),
        }
    }

    if !refreshed {
        let detail = errors
            .last()
            .cloned()
            .unwrap_or_else(|| "no Notion mounts could be refreshed".to_string());
        return Err(CommandError::new(
            "search",
            "metadata_refresh_failed",
            format!("could not refresh Notion metadata before URL search: {detail}"),
        ));
    }

    let store = SqliteStateStore::open(state_root.to_path_buf())
        .map_err(|error| CommandError::new("search", "store_open_failed", error.to_string()))?;
    run_search(&store, options.clone())
        .map_err(|error| CommandError::new("search", error.code(), error.message()))
}

fn prefetch_notion_url_search_result_ancestors(
    state_root: &Path,
    options: &SearchOptions,
    report: &SearchReport,
) -> Result<(), CommandError> {
    let Some(notion_id) = notion_id_from_url(&options.query) else {
        return Ok(());
    };
    if options
        .connector
        .as_deref()
        .is_some_and(|connector| connector != "notion")
    {
        return Ok(());
    }

    let matching_results = report
        .results
        .iter()
        .filter(|result| {
            result.connector == "notion"
                && result.kind != "workspace"
                && notion_id_from_url(&result.remote_id).as_deref() == Some(notion_id.as_str())
        })
        .collect::<Vec<_>>();
    if matching_results.is_empty() {
        return Ok(());
    }

    let store = SqliteStateStore::open(state_root.to_path_buf())
        .map_err(|error| CommandError::new("search", "store_open_failed", error.to_string()))?;
    for result in matching_results {
        let mount_id = MountId::new(result.mount_id.clone());
        let Some(mount) = store
            .get_mount(&mount_id)
            .map_err(|error| CommandError::new("search", "store_error", error.to_string()))?
        else {
            continue;
        };
        if !mount.projection.uses_virtual_filesystem() {
            continue;
        }

        let identifiers = virtual_fs_ancestor_container_identifiers(
            &store,
            &mount_id,
            &RemoteId::new(result.remote_id.clone()),
        )
        .map_err(|error| {
            CommandError::new("search", "ancestor_prefetch_failed", error.to_string())
        })?;
        for container_identifier in identifiers {
            prefetch_virtual_search_container(state_root, &mount, &container_identifier)?;
        }
    }

    Ok(())
}

fn prefetch_virtual_search_container(
    state_root: &Path,
    mount: &MountConfig,
    container_identifier: &str,
) -> Result<(), CommandError> {
    match run_daemon_report::<VirtualFsChildrenReport>(
        state_root,
        &DaemonRequest::FileProviderChildren {
            mount_id: mount.mount_id.0.clone(),
            container_identifier: container_identifier.to_string(),
        },
    ) {
        DaemonReport::Report(_) => Ok(()),
        DaemonReport::Error(error) => Err(CommandError::new("search", error.code, error.message)),
        DaemonReport::Unavailable(DaemonUnavailableReason::TimedOut) => Err(CommandError::new(
            "search",
            "daemon_timeout",
            format!(
                "afsd did not respond within {}ms while enumerating ancestor metadata for search result `{container_identifier}`",
                daemon_request_timeout().as_millis()
            ),
        )
        .with_suggested_command("afs daemon restart")),
        DaemonReport::Unavailable(DaemonUnavailableReason::Disabled)
        | DaemonReport::Unavailable(DaemonUnavailableReason::NotAvailable) => Err(
            CommandError::new(
                "search",
                "daemon_required",
                format!(
                    "mount `{}` uses projection `{}`; Notion URL search must enumerate ancestor metadata through afsd",
                    mount.mount_id.0,
                    mount.projection.as_str()
                ),
            )
            .with_suggested_command("afs daemon restart"),
        ),
    }
}

fn should_refresh_notion_url_search(options: &SearchOptions, report: &SearchReport) -> bool {
    report.results.is_empty()
        && notion_id_from_url(&options.query).is_some()
        && options
            .connector
            .as_deref()
            .is_none_or(|connector| connector == "notion")
}

fn refresh_search_mount_metadata(
    state_root: &Path,
    store: &mut SqliteStateStore,
    mount: &MountConfig,
) -> Result<(), CommandError> {
    match run_daemon_report::<PullReport>(
        state_root,
        &DaemonRequest::Pull {
            path: mount.root.clone(),
        },
    ) {
        DaemonReport::Report(_) => Ok(()),
        DaemonReport::Error(error) => Err(CommandError::new("search", error.code, error.message)),
        DaemonReport::Unavailable(reason) => {
            refresh_search_mount_metadata_direct(state_root, store, mount, reason)
        }
    }
}

fn refresh_search_mount_metadata_direct(
    state_root: &Path,
    store: &mut SqliteStateStore,
    mount: &MountConfig,
    reason: DaemonUnavailableReason,
) -> Result<(), CommandError> {
    match reason {
        DaemonUnavailableReason::TimedOut => {
            return Err(CommandError::new(
                "search",
                "daemon_timeout",
                format!(
                    "afsd did not respond within {}ms while refreshing Notion metadata for search",
                    daemon_mutating_request_timeout().as_millis()
                ),
            )
            .with_suggested_command("afs daemon restart"));
        }
        DaemonUnavailableReason::NotAvailable if mount.projection.uses_virtual_filesystem() => {
            return Err(CommandError::new(
                "search",
                "daemon_required",
                format!(
                    "mount `{}` uses projection `{}`; Notion URL search metadata refresh must run through afsd",
                    mount.mount_id.0,
                    mount.projection.as_str()
                ),
            )
            .with_suggested_command("afs daemon restart"));
        }
        DaemonUnavailableReason::Disabled | DaemonUnavailableReason::NotAvailable => {}
    }

    let credentials = open_credential_store(state_root);
    let connector = resolve_source_for_mount_id(store, credentials.as_ref(), &mount.mount_id)
        .map_err(|error| CommandError::new("search", error.code(), error.message()))?;
    run_pull_with_state_root(store, &connector, mount.root.clone(), Some(state_root))
        .map(|_| ())
        .map_err(|error| CommandError::new("search", error.code(), error.message()))
}

fn templates(args: &[String], json: bool) -> i32 {
    match first_positional(args) {
        Some("list") => match run_template_list() {
            Ok(report) if json => {
                print_json(&report);
                EXIT_SUCCESS
            }
            Ok(report) => {
                print_template_list_report(&report);
                EXIT_SUCCESS
            }
            Err(error) => template_command_error("templates", json, error),
        },
        Some("validate") => {
            let Some(path) = nth_positional(args, 1) else {
                return command_error(
                    json,
                    CommandError::new(
                        "templates",
                        "usage",
                        "usage: afs templates validate <path> [--json]",
                    ),
                    EXIT_USAGE,
                );
            };
            match run_template_validate(PathBuf::from(path)) {
                Ok(report) if json => {
                    print_json(&report);
                    EXIT_SUCCESS
                }
                Ok(report) => {
                    print_template_validate_report(&report);
                    EXIT_SUCCESS
                }
                Err(error) => template_command_error("templates", json, error),
            }
        }
        Some("new") => {
            let Some(pack) = nth_positional(args, 1) else {
                return command_error(
                    json,
                    CommandError::new(
                        "templates",
                        "usage",
                        "usage: afs templates new <pack> <path> [--force] [--json]",
                    ),
                    EXIT_USAGE,
                );
            };
            let Some(path) = nth_positional(args, 2) else {
                return command_error(
                    json,
                    CommandError::new(
                        "templates",
                        "usage",
                        "usage: afs templates new <pack> <path> [--force] [--json]",
                    ),
                    EXIT_USAGE,
                );
            };
            match run_template_new(TemplateNewOptions {
                pack: pack.to_string(),
                path: PathBuf::from(path),
                force: has_flag(args, "--force"),
            }) {
                Ok(report) if json => {
                    print_json(&report);
                    EXIT_SUCCESS
                }
                Ok(report) => {
                    print_template_new_report(&report);
                    EXIT_SUCCESS
                }
                Err(error) => template_command_error("templates", json, error),
            }
        }
        Some("apply") => {
            let Some(pack) = nth_positional(args, 1) else {
                return command_error(
                    json,
                    CommandError::new(
                        "templates",
                        "usage",
                        "usage: afs templates apply <pack> <template> --to <dir> [--title <title>] [--force] [--json]",
                    ),
                    EXIT_USAGE,
                );
            };
            let Some(template) = nth_positional(args, 2) else {
                return command_error(
                    json,
                    CommandError::new(
                        "templates",
                        "usage",
                        "usage: afs templates apply <pack> <template> --to <dir> [--title <title>] [--force] [--json]",
                    ),
                    EXIT_USAGE,
                );
            };
            let Some(target_dir) = flag_value(args, "--to") else {
                return command_error(
                    json,
                    CommandError::new(
                        "templates",
                        "usage",
                        "usage: afs templates apply <pack> <template> --to <dir> [--title <title>] [--force] [--json]",
                    ),
                    EXIT_USAGE,
                );
            };
            match run_template_apply(TemplateApplyOptions {
                pack: pack.to_string(),
                template: template.to_string(),
                target_dir: PathBuf::from(target_dir),
                title: flag_value(args, "--title").map(str::to_string),
                force: has_flag(args, "--force"),
            }) {
                Ok(report) if json => {
                    print_json(&report);
                    EXIT_SUCCESS
                }
                Ok(report) => {
                    print_template_apply_report(&report);
                    EXIT_SUCCESS
                }
                Err(error) => template_command_error("templates", json, error),
            }
        }
        _ => command_error(
            json,
            CommandError::new(
                "templates",
                "usage",
                "usage: afs templates list|validate|new|apply [--json]",
            ),
            EXIT_USAGE,
        ),
    }
}

fn inspect(args: &[String], json: bool) -> i32 {
    let Some(path) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new("inspect", "usage", "usage: afs inspect <path> [--json]"),
            EXIT_USAGE,
        );
    };

    let state_root = default_state_root();
    let store = match SqliteStateStore::open(state_root.clone()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("inspect", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let credentials = open_credential_store(&state_root);
    let connector = match resolve_source_for_path(&store, credentials.as_ref(), path) {
        Ok(connector) => connector,
        Err(error) => return connector_command_error("inspect", json, error),
    };
    let options = InspectOptions {
        path: PathBuf::from(path),
        state_root: Some(state_root),
    };

    match run_inspect(&store, &connector, options) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_inspect_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => inspect_command_error(json, error),
    }
}

fn info(args: &[String], json: bool) -> i32 {
    let state_root = default_state_root();
    let store = match SqliteStateStore::open(state_root.clone()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("info", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let options = InfoOptions {
        path: first_positional(args).map(PathBuf::from),
    };

    match run_info(&store, options) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_info_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => info_command_error(json, error, state_root),
    }
}

fn log(args: &[String], json: bool) -> i32 {
    let store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("log", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let options = LogOptions {
        path: first_positional(args).map(PathBuf::from),
    };

    match run_log(&store, options) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_log_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => history_command_error("log", json, error),
    }
}

fn undo(args: &[String], json: bool) -> i32 {
    let Some(push_id) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new("undo", "usage", "usage: afs undo <push-id> [--json]"),
            EXIT_USAGE,
        );
    };

    let mut store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("undo", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };

    let journal = match store.get_journal(&PushId(push_id.to_string())) {
        Ok(Some(journal)) => journal,
        Ok(None) => {
            return command_error(
                json,
                CommandError::new(
                    "undo",
                    "journal_not_found",
                    format!("journal entry `{push_id}` was not found"),
                ),
                EXIT_USAGE,
            );
        }
        Err(error) => {
            return command_error(
                json,
                CommandError::new("undo", "store_error", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let state_root = default_state_root();
    let credentials = open_credential_store(&state_root);
    let connector =
        match resolve_source_for_mount_id(&store, credentials.as_ref(), &journal.mount_id) {
            Ok(connector) => connector,
            Err(error) => return connector_command_error("undo", json, error),
        };
    let mut undo_applier = ConnectorUndoApplier::new(&connector);

    match run_undo_with_applier(&mut store, push_id, &mut undo_applier) {
        Ok(report) if json => {
            let exit_code = undo_report_exit_code(&report);
            print_json(&report);
            exit_code
        }
        Ok(report) => {
            let exit_code = undo_report_exit_code(&report);
            print_undo_report(&report);
            exit_code
        }
        Err(error) => history_command_error("undo", json, error),
    }
}

fn push(args: &[String], json: bool) -> i32 {
    let Some(path) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new(
                "push",
                "usage",
                "usage: afs push <path> [-y|--yes] [--confirm] [--json]",
            ),
            EXIT_USAGE,
        );
    };

    let options = PushOptions {
        assume_yes: has_flag(args, "-y") || has_flag(args, "--yes"),
        confirm_dangerous: has_flag(args, "--confirm"),
    };
    let state_root = default_state_root();
    let mut store = match SqliteStateStore::open(state_root.clone()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("push", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let target_path = PathBuf::from(path);
    if let Err(error) =
        reconcile_projection_changes("push", &mut store, &state_root, Some(&target_path))
    {
        return command_error(json, error, EXIT_INTERNAL);
    }
    let selection = match select_push_targets(&store, target_path, Some(state_root.clone())) {
        Ok(selection) => selection,
        Err(error) => return push_target_error(json, error),
    };

    if selection.scoped && selection.targets.is_empty() {
        if json {
            print_json(&PushBatchReport::empty(&selection));
        } else {
            println!("nothing to push");
        }
        return EXIT_SUCCESS;
    }

    let mut reports = Vec::new();
    let stderr_is_terminal = io::stderr().is_terminal();
    for target in &selection.targets {
        let target_label = target.display().to_string();
        let spinner_config =
            spinner_config_for_command("push", &target_label, json, stderr_is_terminal);
        let report = match with_terminal_spinner(spinner_config.clone(), || {
            run_push_target_command(&mut store, &state_root, target.clone(), options.clone())
        }) {
            Ok(report) => report,
            Err(error) => {
                return command_error(json, error.payload, error.exit_code);
            }
        };

        let report = if should_prompt_for_push_confirmation(
            &report,
            &options,
            json,
            io::stdin().is_terminal(),
        ) {
            print_diff_report_fields(&report.validation, report.plan.as_ref());
            match prompt_for_push_confirmation(&mut io::stdin().lock(), &mut io::stdout()) {
                Ok(true) => {
                    let mut approved = options.clone();
                    approved.assume_yes = true;
                    match with_terminal_spinner(spinner_config, || {
                        run_push_target_command(&mut store, &state_root, target.clone(), approved)
                    }) {
                        Ok(report) => report,
                        Err(error) => {
                            return command_error(json, error.payload, error.exit_code);
                        }
                    }
                }
                Ok(false) => {
                    if !json {
                        println!("push cancelled");
                    }
                    return push_report_exit_code(&report);
                }
                Err(error) => {
                    return command_error(
                        json,
                        CommandError::new("push", "stdin_read_failed", error.to_string()),
                        EXIT_INTERNAL,
                    );
                }
            }
        } else {
            report
        };

        reports.push(report);
    }

    let exit_code = push_reports_exit_code(&reports);
    if json {
        if !selection.scoped && reports.len() == 1 {
            print_json(&reports[0]);
        } else {
            print_json(&PushBatchReport::from_reports(&selection, reports));
        }
    } else {
        for report in &reports {
            if selection.scoped && reports.len() > 1 {
                println!("pushing {}", report.path);
            }
            print_push_report(report);
        }
    }

    exit_code
}

#[derive(Debug)]
struct PushCommandError {
    payload: CommandError,
    exit_code: i32,
}

impl PushCommandError {
    fn new(code: impl Into<String>, message: impl Into<String>, exit_code: i32) -> Self {
        Self {
            payload: CommandError::new("push", code, message),
            exit_code,
        }
    }

    fn from_connector(error: ConnectorResolveError) -> Self {
        let exit_code = match error.code() {
            "mount_not_found" => EXIT_USAGE,
            "missing_connection"
            | "auth_required"
            | "connection_revoked"
            | "auth_profile_unavailable"
            | "credential_store_unavailable" => EXIT_INTERNAL,
            _ => EXIT_INTERNAL,
        };
        let mut payload = CommandError::new("push", error.code(), error.message());
        if let Some(suggested_command) = error.suggested_command() {
            payload = payload.with_suggested_command(suggested_command);
        }
        Self { payload, exit_code }
    }

    fn from_afs(error: AfsError) -> Self {
        Self::new(
            afs_error_code(&error),
            error.to_string(),
            afs_error_exit_code(&error),
        )
    }
}

#[derive(Serialize)]
struct PushBatchReport {
    ok: bool,
    command: &'static str,
    path: String,
    scoped: bool,
    target_count: usize,
    reports: Vec<PushReport>,
}

impl PushBatchReport {
    fn empty(selection: &crate::push::PushTargetSelection) -> Self {
        Self {
            ok: true,
            command: "push",
            path: selection.requested_path.display().to_string(),
            scoped: selection.scoped,
            target_count: 0,
            reports: Vec::new(),
        }
    }

    fn from_reports(
        selection: &crate::push::PushTargetSelection,
        reports: Vec<PushReport>,
    ) -> Self {
        Self {
            ok: reports.iter().all(|report| report.ok),
            command: "push",
            path: selection.requested_path.display().to_string(),
            scoped: selection.scoped,
            target_count: reports.len(),
            reports,
        }
    }
}

fn run_push_target_command(
    store: &mut SqliteStateStore,
    state_root: &Path,
    target_path: PathBuf,
    options: PushOptions,
) -> Result<PushReport, PushCommandError> {
    match run_daemon_report::<PushJobReport>(
        state_root,
        &DaemonRequest::Push {
            path: target_path.clone(),
            assume_yes: options.assume_yes,
            confirm_dangerous: options.confirm_dangerous,
        },
    ) {
        DaemonReport::Report(report) => return Ok(PushReport::from_daemon(report)),
        DaemonReport::Unavailable(DaemonUnavailableReason::TimedOut) => {
            return Err(PushCommandError::new(
                "daemon_timeout",
                format!(
                    "afsd did not respond within {}ms after the push request was submitted; refusing direct fallback to avoid duplicate remote writes",
                    daemon_mutating_request_timeout().as_millis()
                ),
                EXIT_INTERNAL,
            ));
        }
        DaemonReport::Unavailable(reason) => warn_daemon_fallback("push", reason),
        DaemonReport::Error(error) => {
            return Err(PushCommandError {
                payload: CommandError::new("push", error.code, error.message),
                exit_code: error.exit_code,
            });
        }
    }

    let credentials = open_credential_store(state_root);
    let connector = resolve_source_for_path(store, credentials.as_ref(), &target_path)
        .map_err(PushCommandError::from_connector)?;
    run_push_with_daemon_at_state_root(store, &connector, target_path, options, Some(state_root))
        .map_err(PushCommandError::from_afs)
}

fn should_prompt_for_push_confirmation(
    report: &PushReport,
    options: &PushOptions,
    json: bool,
    stdin_is_terminal: bool,
) -> bool {
    report.action == "confirm_plan" && !options.assume_yes && !json && stdin_is_terminal
}

fn prompt_for_push_confirmation<R, W>(input: &mut R, output: &mut W) -> io::Result<bool>
where
    R: BufRead,
    W: Write,
{
    loop {
        write!(output, "Proceed with push? [y/N] ")?;
        output.flush()?;

        let mut answer = String::new();
        input.read_line(&mut answer)?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "" | "n" | "no" => return Ok(false),
            _ => {
                writeln!(output, "Please answer y or n.")?;
            }
        }
    }
}

fn push_reports_exit_code(reports: &[PushReport]) -> i32 {
    reports
        .iter()
        .map(push_report_exit_code)
        .find(|exit_code| *exit_code != EXIT_SUCCESS)
        .unwrap_or(EXIT_SUCCESS)
}

fn push_target_error(json: bool, error: StatusError) -> i32 {
    let exit_code = match &error {
        StatusError::MountNotFound(_)
        | StatusError::Store(afs_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
        StatusError::CurrentDir(_) | StatusError::Store(_) => EXIT_INTERNAL,
    };
    command_error(
        json,
        CommandError::new("push", error.code(), error.message()),
        exit_code,
    )
}

fn diff(args: &[String], json: bool) -> i32 {
    let Some(path) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new("diff", "usage", "usage: afs diff <path> [--json]"),
            EXIT_USAGE,
        );
    };

    let state_root = default_state_root();
    let mut store = match SqliteStateStore::open(state_root.clone()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("diff", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let target_path = PathBuf::from(path);
    if let Err(error) =
        reconcile_projection_changes("diff", &mut store, &state_root, Some(&target_path))
    {
        return command_error(json, error, EXIT_INTERNAL);
    }

    match run_diff_with_state_root(&store, target_path, Some(&state_root)) {
        Ok(report) if json => {
            let exit_code = diff_report_exit_code(&report);
            print_json(&report);
            exit_code
        }
        Ok(report) => {
            let exit_code = diff_report_exit_code(&report);
            print_diff_report(&report);
            exit_code
        }
        Err(error) => {
            let exit_code = diff_error_exit_code(&error);
            command_error(
                json,
                CommandError::new("diff", error.code(), error.message()),
                exit_code,
            )
        }
    }
}

fn print_log_report(report: &LogReport) {
    if report.entries.is_empty() {
        println!("no journal entries");
        return;
    }

    for (index, entry) in report.entries.iter().enumerate() {
        if index > 0 {
            println!();
        }
        println!("push {}", entry.push_id);
        println!("  status: {}", entry.status);
        println!("  mount: {}", entry.mount_id);
        println!("  entities: {}", entry.remote_ids.join(", "));
        if let Some(failure) = &entry.failure {
            println!("  failure: {failure}");
        }
        println!(
            "  summary: {} updated, {} media updated, {} created, {} moved, {} archived",
            entry.plan_summary.blocks_updated,
            entry.plan_summary.media_updated,
            entry.plan_summary.blocks_created,
            entry.plan_summary.blocks_moved,
            entry.plan_summary.blocks_archived
        );
        println!("  operations: {}", entry.operation_count);
    }
}

fn print_undo_report(report: &UndoReport) {
    if report.ok {
        println!("{}", report.message);
    } else {
        println!("undo blocked for {}: {}", report.push_id, report.message);
        if let Some(plan) = &report.undo_plan {
            println!(
                "  undo plan: {} ({} operations, {} unsupported)",
                plan.status,
                plan.operations.len(),
                plan.unsupported.len()
            );
        }
    }
}

fn print_push_report(report: &PushReport) {
    match report.action.as_str() {
        "noop" => println!("nothing to push"),
        "reconciled" => println!(
            "push {} reconciled (via {})",
            report.push_id.as_deref().unwrap_or("<unknown>"),
            report.via
        ),
        "fix_validation" => print_diff_report_fields(&report.validation, report.plan.as_ref()),
        "confirm_plan" => println!("push requires confirmation; rerun with -y or --yes"),
        "confirm_dangerous_plan" => println!("dangerous push requires --confirm"),
        "read_only_blocked" => println!("push blocked: mount is read-only"),
        "apply_not_implemented" => {
            println!(
                "{}",
                report
                    .message
                    .as_deref()
                    .unwrap_or("connector apply is not implemented yet")
            );
        }
        "unsupported_operations" => {
            println!(
                "{}",
                report
                    .message
                    .as_deref()
                    .unwrap_or("connector cannot apply one or more planned operations")
            );
            if let Some(suggested_fix) = &report.suggested_fix {
                println!("  suggested_fix: {suggested_fix}");
            }
        }
        "apply_failed" => {
            println!(
                "{}",
                report
                    .message
                    .as_deref()
                    .unwrap_or("connector apply failed")
            );
            if let Some(suggested_fix) = &report.suggested_fix {
                println!("  suggested_fix: {suggested_fix}");
            }
        }
        _ => println!("push stopped: {}", report.action),
    }
}

fn print_mount_report(report: &MountReport) {
    println!(
        "mounted {} at {} ({})",
        report.mount_id, report.root, report.connector
    );
    if let Some(connection_id) = &report.connection_id {
        println!("connection: {connection_id}");
    }
    println!(
        "agent guidance: {} {}, {} {}",
        report.guidance.agents_md.action.as_str(),
        report.guidance.agents_md.path,
        report.guidance.claude_md.action.as_str(),
        report.guidance.claude_md.path
    );
}

fn print_connect_report(report: &ConnectReport) {
    let account = report
        .account_label
        .as_deref()
        .or(report.workspace_name.as_deref())
        .unwrap_or("Notion");
    println!(
        "connected notion as \"{}\" (connection: {})",
        account, report.connection_id
    );
}

fn print_connections_report(report: &ConnectionsReport) {
    if report.connections.is_empty() {
        println!("no connections");
        return;
    }

    for connection in &report.connections {
        let label = connection
            .account_label
            .as_deref()
            .or(connection.workspace_name.as_deref())
            .unwrap_or("-");
        println!(
            "{}  {}  {}  {}  {}",
            connection.connection_id,
            connection
                .profile_id
                .as_deref()
                .unwrap_or("profile:unknown"),
            connection.connector,
            connection.status,
            label
        );
    }
}

fn print_profiles_report(report: &ProfilesReport) {
    if report.profiles.is_empty() {
        println!("no profiles");
        return;
    }

    for profile in &report.profiles {
        println!(
            "{}  {}  {}  {}  {}",
            profile.profile_id,
            profile.connector,
            profile.auth_kind,
            profile.status,
            profile.connector_version
        );
    }
}

fn print_connection_show_report(report: &ConnectionShowReport) {
    let connection = &report.connection;
    println!("connection: {}", connection.connection_id);
    if let Some(profile_id) = &connection.profile_id {
        println!("  profile: {profile_id}");
    }
    println!("  connector: {}", connection.connector);
    println!("  status: {}", connection.status);
    println!("  auth_kind: {}", connection.auth_kind);
    if let Some(account_label) = &connection.account_label {
        println!("  account: {account_label}");
    }
    if let Some(workspace_name) = &connection.workspace_name {
        println!("  workspace: {workspace_name}");
    }
}

fn print_disconnect_report(report: &DisconnectReport) {
    println!("disconnected {} ({})", report.connection_id, report.status);
}

fn print_pull_report(report: &PullReport) {
    if !report.conflicts.is_empty() {
        let skipped_without_conflicts = report.skipped_dirty.saturating_sub(report.conflicts.len());
        println!(
            "pull completed with {} conflicted file(s); {} dirty file(s) skipped, {} hydrated, {} stubbed, {} enumerated (via {})",
            report.conflicts.len(),
            skipped_without_conflicts,
            report.hydrated,
            report.stubbed,
            report.enumerated,
            report.via
        );
        println!("  conflicted:");
        for conflict in &report.conflicts {
            println!("    {}", conflict.path);
        }
        println!("  next: resolve the conflict markers in the file(s)");
        if report.conflicts.len() == 1 {
            let path = shell_quote(&report.conflicts[0].path);
            println!("  then: afs push {path} -y");
        } else {
            println!("  then: run `afs push <file> -y` for each resolved file");
        }
    } else if report.skipped_dirty > 0 {
        println!(
            "pull skipped {} dirty file(s); {} hydrated, {} stubbed, {} enumerated (via {})",
            report.skipped_dirty, report.hydrated, report.stubbed, report.enumerated, report.via
        );
    } else {
        println!(
            "pull complete: {} hydrated, {} stubbed, {} enumerated (via {})",
            report.hydrated, report.stubbed, report.enumerated, report.via
        );
    }
}

fn shell_quote(value: &str) -> String {
    if value.chars().all(|character| {
        character.is_ascii_alphanumeric()
            || matches!(character, '/' | '.' | '_' | '-' | '~' | ':' | '=')
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn print_restore_report(report: &RestoreReport) {
    println!("restored {}", report.path);
}

fn print_status_report(report: &StatusReport) {
    if report.mounts.is_empty() {
        println!("no mounts");
        return;
    }

    let mut printed_entries = 0;
    for mount in &report.mounts {
        for entry in &mount.entries {
            if matches!(
                entry.sync_state,
                StatusSyncState::AllSynced | StatusSyncState::CheckingFreshness
            ) && entry.pending_journal_count == 0
                && entry.failed_journal_count == 0
            {
                continue;
            }

            printed_entries += 1;
            println!("{}  {}", mount.mount_id, entry.path);
            println!(
                "  state: {}  sync: {}  hydration: {}",
                entry.state.as_str(),
                entry.sync_state.as_str(),
                entry.hydration
            );
            for issue in &entry.issues {
                if issue.code == "last_failure" {
                    println!("  last_failure: {}", issue.message);
                } else {
                    println!("  issue: {} - {}", issue.code, issue.message);
                }
            }
        }
    }

    if printed_entries == 0 {
        let checking = if report.summary.checking_freshness > 0 {
            format!(
                " (checking freshness for {} entr{})",
                report.summary.checking_freshness,
                if report.summary.checking_freshness == 1 {
                    "y"
                } else {
                    "ies"
                }
            )
        } else {
            String::new()
        };
        println!(
            "status clean: {} tracked entr{}{}",
            report.summary.total,
            if report.summary.total == 1 {
                "y"
            } else {
                "ies"
            },
            checking
        );
    } else {
        println!(
            "summary: {} clean, {} stub, {} dirty, {} conflicted, {} missing, {} error",
            report.summary.clean,
            report.summary.stub,
            report.summary.dirty,
            report.summary.conflicted,
            report.summary.missing,
            report.summary.error
        );
        println!(
            "sync: {} remote updates, {} pending local, {} review needed, {} conflicted, {} checking",
            report.summary.remote_update_available,
            report.summary.pending_local_changes,
            report.summary.review_needed,
            report.summary.sync_conflicted,
            report.summary.checking_freshness
        );
    }
}

fn print_search_report(report: &SearchReport) {
    if report.results.is_empty() {
        println!("no local matches for {:?}", report.query);
        return;
    }

    for result in &report.results {
        println!(
            "{}  {}  {}  {}",
            result.title, result.kind, result.state, result.path
        );
        println!(
            "  mount: {}  connector: {}  remote: {}",
            result.mount_id, result.connector, result.remote_id
        );
        println!("  path: {}", result.absolute_path);
        if !result.safety.agent_readable {
            println!("  safety: {}", result.safety.labels.join(", "));
        }
        if result.remote.changed {
            let state = if result.remote.deleted {
                "deleted"
            } else {
                "changed"
            };
            println!("  remote: {state}");
        }
    }
}

fn print_template_list_report(report: &TemplateListReport) {
    if report.packs.is_empty() {
        println!("no template packs available");
        return;
    }

    for pack in &report.packs {
        println!("{}  {}  {}", pack.id, pack.version, pack.name);
        if let Some(description) = &pack.description {
            println!("  {description}");
        }
        if !pack.requires.connectors.is_empty() {
            println!("  connectors: {}", pack.requires.connectors.join(", "));
        }
        if !pack.outputs.is_empty() {
            println!("  outputs: {}", pack.outputs.join(", "));
        }
    }
}

fn print_template_validate_report(report: &TemplateValidateReport) {
    println!(
        "template pack valid: {} {}",
        report.pack.id, report.pack.version
    );
    println!("  path: {}", report.path);
    if !report.pack.requires.connectors.is_empty() {
        println!(
            "  connectors: {}",
            report.pack.requires.connectors.join(", ")
        );
    }
}

fn print_template_new_report(report: &TemplateNewReport) {
    println!(
        "created template workspace {} from {} {}",
        report.path, report.pack.id, report.pack.version
    );
    println!("  files: {}", report.files_written.len());
}

fn print_template_apply_report(report: &TemplateApplyReport) {
    println!(
        "created draft {} from {}/{}",
        report.path, report.pack.id, report.template
    );
    for next in &report.suggested_next {
        println!("  next: {next}");
    }
}

fn print_inspect_report(report: &InspectReport) {
    println!("inspect {}", report.path);
    println!("  mount: {}  entity: {}", report.mount_id, report.entity_id);
    println!("  title: {}", report.title);
    if let Some(version) = &report.synced_tree_version {
        println!("  Synced Tree version: {version}");
    }
    if let Some(version) = &report.remote_tree_version {
        println!("  Remote Tree version: {version}");
    }
    if report.local_read_path != report.path {
        println!("  local cache: {}", report.local_read_path);
    }
    println!(
        "  state: {}  action: {}",
        report.explanation.state.as_str(),
        report.explanation.action.as_str()
    );
    println!(
        "  local: {}",
        inspect_side_summary(&report.explanation.local)
    );
    println!(
        "  remote: {}",
        inspect_side_summary(&report.explanation.remote)
    );
    for issue in &report.explanation.issues {
        println!("  issue: {} - {}", issue.code, issue.message);
    }
}

fn inspect_side_summary(side: &afs_core::explain::RemoteChangeSide) -> String {
    if let Some(issue) = &side.issue {
        return format!("needs review ({} - {})", issue.code, issue.message);
    }

    let operations = side
        .plan
        .as_ref()
        .map(|plan| plan.operations.len())
        .unwrap_or(0);
    format!(
        "{} ({operations} operation{})",
        if side.changed { "changed" } else { "unchanged" },
        plural(operations)
    )
}

fn print_info_report(report: &InfoReport) {
    println!("Path: {}", report.target);
    println!(
        "Mount: {} ({})",
        report.mount.mount_id, report.mount.connector
    );
    println!("Root: {}", report.mount.root);
    println!("Role: {}", report.subject.role.label());
    println!("Source: {}", report.subject.source);

    if let Some(remote_root_id) = &report.mount.remote_root_id {
        println!("Remote root ID: {remote_root_id}");
    }
    if let Some(entity) = &report.subject.entity {
        println!("Title: {}", entity.title);
        println!("Remote ID: {}", entity.entity_id);
        println!("Entity path: {}", entity.path);
        println!("Hydration: {}", entity.hydration);
        if let Some(remote_edited_at) = &entity.remote_edited_at {
            println!("Synced Tree version: {remote_edited_at}");
        }
    }
    if let Some(schema_path) = &report.subject.schema_path {
        println!("Schema: {schema_path}");
    }

    println!(
        "Children: {} page{}, {} database{}, {} director{}, {} asset{}, {} unknown",
        report.children.pages,
        plural(report.children.pages),
        report.children.databases,
        plural(report.children.databases),
        report.children.directories,
        if report.children.directories == 1 {
            "y"
        } else {
            "ies"
        },
        report.children.assets,
        plural(report.children.assets),
        report.children.unknown,
    );
    println!("Subtree entities: {}", report.children.subtree);
    println!(
        "Journals: {} pending, {} failed",
        report.journals.pending, report.journals.failed
    );
    println!(
        "Write mode: {}",
        if report.mount.read_only {
            "read-only"
        } else {
            "read-write"
        }
    );

    if !report.suggestions.is_empty() {
        println!("Next: {}", report.suggestions.join("; "));
    }
}

fn print_daemon_report(report: &DaemonControlReport) {
    println!("{}", report.message);
    println!("  state: {}", report.state.as_str());
    println!("  manager: {}", report.manager.as_str());
    println!("  state root: {}", report.state_root);
    println!("  socket: {}", report.socket);
    if let Some(reload) = &report.reload {
        println!(
            "  reload: +{} -{} unchanged {}",
            reload.added, reload.removed, reload.unchanged
        );
    }
    if let Some(status) = &report.daemon_status {
        println!(
            "  build: {} ({})",
            status.build.version, status.build.build_id
        );
        println!("  watched mounts: {}", status.watches.watched_mounts);
        println!(
            "  jobs: active={}, pending={}, hydration={}, freshness={}",
            status.runtime.active_job,
            status.runtime.pending_requests,
            status.runtime.pending_hydrations,
            status.runtime.pending_freshness
        );
        println!(
            "  freshness: ready={}, deferred={}, ready_budget={}, total_budget={}",
            status.runtime.ready_freshness,
            status.runtime.deferred_freshness,
            status.runtime.ready_freshness_budget_units,
            status.runtime.freshness_budget_units
        );
        if let Some(active) = &status.runtime.active_job_detail {
            println!(
                "  active job: kind={} target={} elapsed={}ms",
                active.kind,
                active.target.as_deref().unwrap_or("-"),
                active.elapsed_ms
            );
        }
        println!("  scheduler: {}", status.runtime.scheduler_mode);
    }
    if let Some(log) = &report.stderr_log {
        println!("  log: {log}");
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn run_platform_file_provider_helper(
    json: bool,
    action: &str,
    args: Vec<String>,
    mount_id: Option<String>,
) -> i32 {
    if std::env::consts::OS == "windows" {
        return run_windows_cloud_files_helper(json, action, args, mount_id);
    }

    run_file_provider_helper(json, action, args, mount_id)
}

fn run_file_provider_helper(
    json: bool,
    action: &str,
    args: Vec<String>,
    mount_id: Option<String>,
) -> i32 {
    let helper_report = match file_provider_helper::run_macos_file_provider_helper(action, args) {
        Ok(report) => report,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("file-provider", error.code(), error.message()),
                EXIT_INTERNAL,
            );
        }
    };

    let report = FileProviderCommandReport {
        ok: true,
        command: "file-provider",
        action: action.to_string(),
        mount_id,
        helper: helper_report.helper.display().to_string(),
        helper_report: helper_report.helper_report,
    };

    if json {
        print_json(&report);
    } else {
        print_file_provider_report(&report);
    }
    EXIT_SUCCESS
}

fn run_windows_cloud_files_register(json: bool, mount: &MountConfig) -> i32 {
    let state_root = default_state_root();
    let display_name = file_provider_display_name(mount);
    let helper_report = match file_provider_helper::register_windows_cloud_files_sync_root(
        &state_root,
        mount,
        &display_name,
    ) {
        Ok(report) => report,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("file-provider", error.code(), error.message()),
                EXIT_INTERNAL,
            );
        }
    };

    file_provider_helper_success_report(
        json,
        "register",
        Some(mount.mount_id.0.clone()),
        helper_report,
    )
}

fn run_windows_cloud_files_run(json: bool, mount: &MountConfig) -> i32 {
    let state_root = default_state_root();
    let helper_report =
        match file_provider_helper::run_windows_cloud_files_provider(&state_root, mount) {
            Ok(report) => report,
            Err(error) => {
                return command_error(
                    json,
                    CommandError::new("file-provider", error.code(), error.message()),
                    EXIT_INTERNAL,
                );
            }
        };

    file_provider_helper_success_report(json, "run", Some(mount.mount_id.0.clone()), helper_report)
}

fn run_windows_cloud_files_lifecycle(
    json: bool,
    mount: &MountConfig,
    action: file_provider_helper::WindowsCloudFilesLifecycleAction,
) -> i32 {
    let state_root = default_state_root();
    let display_name = file_provider_display_name(mount);
    let helper_report = match file_provider_helper::run_windows_cloud_files_lifecycle(
        &state_root,
        mount,
        &display_name,
        action,
    ) {
        Ok(report) => report,
        Err(error) => {
            return command_error(
                json,
                windows_cloud_files_command_error(error),
                EXIT_INTERNAL,
            );
        }
    };

    file_provider_helper_success_report(
        json,
        action.as_str(),
        Some(mount.mount_id.0.clone()),
        helper_report,
    )
}

fn run_windows_cloud_files_open(json: bool, mount: &MountConfig) -> i32 {
    let helper_report = match file_provider_helper::open_windows_cloud_files_sync_root(mount) {
        Ok(report) => report,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("file-provider", error.code(), error.message()),
                EXIT_INTERNAL,
            );
        }
    };

    file_provider_helper_success_report(json, "open", Some(mount.mount_id.0.clone()), helper_report)
}

fn run_windows_cloud_files_unregister(json: bool, mount_id: &str) -> i32 {
    let state_root = default_state_root();
    let helper_report =
        match file_provider_helper::unregister_windows_cloud_files_sync_root(&state_root, mount_id)
        {
            Ok(report) => report,
            Err(error) => {
                return command_error(
                    json,
                    CommandError::new("file-provider", error.code(), error.message()),
                    EXIT_INTERNAL,
                );
            }
        };

    file_provider_helper_success_report(
        json,
        "unregister",
        Some(mount_id.to_string()),
        helper_report,
    )
}

fn windows_cloud_files_state_args_for_platform() -> Vec<String> {
    if std::env::consts::OS == "windows" {
        vec![
            "--state-dir".to_string(),
            absolute_command_path(&default_state_root())
                .display()
                .to_string(),
        ]
    } else {
        Vec::new()
    }
}

fn absolute_command_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    std::env::current_dir()
        .map(|current_dir| current_dir.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn run_windows_cloud_files_helper(
    json: bool,
    action: &str,
    args: Vec<String>,
    mount_id: Option<String>,
) -> i32 {
    let helper_report = match file_provider_helper::run_windows_cloud_files_helper(action, args) {
        Ok(report) => report,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("file-provider", error.code(), error.message()),
                EXIT_INTERNAL,
            );
        }
    };

    file_provider_helper_success_report(json, action, mount_id, helper_report)
}

fn file_provider_helper_success_report(
    json: bool,
    action: &str,
    mount_id: Option<String>,
    helper_report: file_provider_helper::FileProviderHelperReport,
) -> i32 {
    let report = FileProviderCommandReport {
        ok: true,
        command: "file-provider",
        action: action.to_string(),
        mount_id,
        helper: helper_report.helper.display().to_string(),
        helper_report: helper_report.helper_report,
    };

    if json {
        print_json(&report);
    } else {
        print_file_provider_report(&report);
    }
    EXIT_SUCCESS
}

#[cfg(target_os = "linux")]
fn run_linux_fuse_register(json: bool, mount: &MountConfig) -> i32 {
    let state_root = default_state_root();
    let registration = match file_provider_helper::register_linux_fuse_mount(&state_root, mount) {
        Ok(report) => report,
        Err(error) => {
            return command_error(json, linux_fuse_command_error(error), EXIT_INTERNAL);
        }
    };

    let report = FileProviderCommandReport {
        ok: true,
        command: "file-provider",
        action: "register".to_string(),
        mount_id: Some(mount.mount_id.0.clone()),
        helper: "systemctl --user".to_string(),
        helper_report: serde_json::json!({
            "message": format!("Linux FUSE mount registered for `{}`", mount.mount_id.0),
            "service": registration.service,
            "unit_path": registration.unit_path.display().to_string(),
            "mountpoint": registration.mountpoint.display().to_string(),
            "afs_fuse": registration.afs_fuse.display().to_string(),
        }),
    };
    if json {
        print_json(&report);
    } else {
        print_file_provider_report(&report);
    }
    EXIT_SUCCESS
}

#[cfg(not(target_os = "linux"))]
fn run_linux_fuse_register(json: bool, mount: &MountConfig) -> i32 {
    command_error(
        json,
        CommandError::new(
            "file-provider",
            "unsupported_platform",
            format!(
                "linux_fuse registration is only supported on Linux; mount `{}` cannot be registered here",
                mount.mount_id.0
            ),
        ),
        EXIT_USAGE,
    )
}

#[cfg(target_os = "linux")]
fn open_path_for_linux_fuse(json: bool, mount: &MountConfig) -> i32 {
    match ProcessCommand::new("xdg-open").arg(&mount.root).spawn() {
        Ok(_) => {
            let report = FileProviderCommandReport {
                ok: true,
                command: "file-provider",
                action: "open".to_string(),
                mount_id: Some(mount.mount_id.0.clone()),
                helper: "xdg-open".to_string(),
                helper_report: serde_json::json!({
                    "message": format!("opened {}", mount.root.display()),
                    "mountpoint": mount.root.display().to_string(),
                }),
            };
            if json {
                print_json(&report);
            } else {
                print_file_provider_report(&report);
            }
            EXIT_SUCCESS
        }
        Err(error) => command_error(
            json,
            CommandError::new("file-provider", "helper_failed", error.to_string()),
            EXIT_INTERNAL,
        ),
    }
}

#[cfg(not(target_os = "linux"))]
fn open_path_for_linux_fuse(json: bool, mount: &MountConfig) -> i32 {
    command_error(
        json,
        CommandError::new(
            "file-provider",
            "unsupported_platform",
            format!(
                "linux_fuse open is only supported on Linux; mount `{}` cannot be opened here",
                mount.mount_id.0
            ),
        ),
        EXIT_USAGE,
    )
}

#[cfg(target_os = "linux")]
fn run_linux_fuse_unregister(json: bool, mount: Option<&MountConfig>, target: &str) -> i32 {
    if let Some(mount) = mount
        && let Err(error) = validate_virtual_projection_registration(mount, "linux")
    {
        return command_error(json, error, EXIT_USAGE);
    }
    let mount_id = mount
        .map(|mount| mount.mount_id.0.clone())
        .unwrap_or_else(|| target.to_string());
    let unit_name = file_provider_helper::linux_fuse_unit_name(&mount_id);
    let unit_path = match file_provider_helper::linux_fuse_unit_path(&unit_name) {
        Ok(path) => path,
        Err(error) => return command_error(json, linux_fuse_command_error(error), EXIT_INTERNAL),
    };

    let _ = file_provider_helper::run_systemctl_user(&["disable", "--now", &unit_name]);
    if let Some(mount) = mount {
        let _ = ProcessCommand::new("fusermount3")
            .arg("-uz")
            .arg(&mount.root)
            .output();
    }
    let _ = std::fs::remove_file(&unit_path);
    if let Err(error) = file_provider_helper::run_systemctl_user(&["daemon-reload"]) {
        return command_error(json, linux_fuse_command_error(error), EXIT_INTERNAL);
    }

    let report = FileProviderCommandReport {
        ok: true,
        command: "file-provider",
        action: "unregister".to_string(),
        mount_id: Some(mount_id.clone()),
        helper: "systemctl --user".to_string(),
        helper_report: serde_json::json!({
            "message": format!("Linux FUSE mount unregistered for `{mount_id}`"),
            "service": unit_name,
            "unit_path": unit_path.display().to_string(),
        }),
    };
    if json {
        print_json(&report);
    } else {
        print_file_provider_report(&report);
    }
    EXIT_SUCCESS
}

#[cfg(not(target_os = "linux"))]
fn run_linux_fuse_unregister(json: bool, _mount: Option<&MountConfig>, target: &str) -> i32 {
    command_error(
        json,
        CommandError::new(
            "file-provider",
            "unsupported_platform",
            format!("linux_fuse unregister is only supported on Linux for `{target}`"),
        ),
        EXIT_USAGE,
    )
}

#[cfg(target_os = "linux")]
fn linux_fuse_command_error(
    error: file_provider_helper::LinuxFuseRegistrationError,
) -> CommandError {
    CommandError::new("file-provider", error.code(), error.message())
}

fn windows_cloud_files_command_error(
    error: file_provider_helper::WindowsCloudFilesHelperError,
) -> CommandError {
    let command_error = CommandError::new("file-provider", error.code(), error.message());
    match error {
        file_provider_helper::WindowsCloudFilesHelperError::DaemonNotRunning => {
            command_error.with_suggested_command("afs daemon start")
        }
        _ => command_error,
    }
}

fn print_file_provider_report(report: &FileProviderCommandReport) {
    if report.action == "list" {
        if let Some(roots) = report.helper_report.get("roots").and_then(Value::as_array) {
            if roots.is_empty() {
                println!("no file provider domains");
                return;
            }
            for root in roots {
                let mount_id = root
                    .get("mount_id")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown>");
                let display_name = root
                    .get("display_name")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown>");
                let path = root
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown>");
                println!("{mount_id}\t{display_name}\t{path}");
            }
            return;
        }
        let Some(domains) = report
            .helper_report
            .get("domains")
            .and_then(Value::as_array)
        else {
            println!("no file provider domains");
            return;
        };
        if domains.is_empty() {
            println!("no file provider domains");
            return;
        }
        for domain in domains {
            let identifier = domain
                .get("identifier")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let display_name = domain
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            println!("{identifier}\t{display_name}");
        }
        return;
    }

    if let Some(message) = report
        .helper_report
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
    {
        println!("{message}");
    } else {
        println!("file provider {} complete", report.action);
    }
}

fn resolve_mount_target(store: &SqliteStateStore, target: &str) -> Result<MountConfig, String> {
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("failed to load mounts: {error}"))?;
    if let Some(mount) = mounts
        .iter()
        .find(|mount| mount.mount_id.0 == target)
        .cloned()
    {
        return Ok(mount);
    }

    let target_path = absolute_path(Path::new(target))
        .map_err(|error| format!("failed to resolve `{target}`: {error}"))?;
    mounts
        .into_iter()
        .filter(|mount| target_path.starts_with(&mount.root))
        .max_by_key(|mount| mount.root.components().count())
        .ok_or_else(|| format!("no AgentFS mount matches `{target}`"))
}

fn absolute_path(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(absolute.canonicalize().unwrap_or(absolute))
}

fn file_provider_display_name(mount: &MountConfig) -> String {
    file_provider_helper::macos_file_provider_display_name(&mount.root, &mount.mount_id.0)
}

fn stub(command: &str, json: bool) -> i32 {
    if json {
        println!("{{\"ok\":false,\"command\":\"{command}\",\"error\":\"not_implemented\"}}");
    } else {
        println!("afs {command}: not implemented yet");
    }

    EXIT_SUCCESS
}

fn print_diff_report(report: &crate::diff::DiffReport) {
    print_diff_report_fields(&report.validation, report.plan.as_ref());
}

fn print_diff_report_fields(
    validation: &[crate::diff::ValidationIssueOutput],
    plan: Option<&crate::diff::PushPlanOutput>,
) {
    if !validation.is_empty() {
        for issue in validation {
            match issue.line {
                Some(line) => println!(
                    "{}:{}: {} ({})",
                    issue.file, line, issue.message, issue.code
                ),
                None => println!("{}: {} ({})", issue.file, issue.message, issue.code),
            }
        }
        return;
    }

    let Some(plan) = plan else {
        println!("no plan");
        return;
    };

    println!(
        "{} blocks updated, {} media updated, {} created, {} moved, {} archived",
        plan.summary.blocks_updated,
        plan.summary.media_updated,
        plan.summary.blocks_created,
        plan.summary.blocks_moved,
        plan.summary.blocks_archived
    );
}

fn read_connect_token(args: &[String], json: bool) -> Result<String, CommandError> {
    let mut token = String::new();
    if has_flag(args, "--token-stdin") {
        io::stdin().read_to_string(&mut token).map_err(|error| {
            CommandError::new("connect", "stdin_read_failed", error.to_string())
        })?;
    } else {
        if !json {
            eprint!("Paste Notion token: ");
        }
        io::stdin().read_line(&mut token).map_err(|error| {
            CommandError::new("connect", "stdin_read_failed", error.to_string())
        })?;
    }

    Ok(token.trim().to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NotionOAuthCliConfig {
    client_id: String,
    client_secret: String,
    redirect_uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NotionOAuthBrokerCliConfig {
    broker_url: String,
    redirect_uri: String,
}

fn notion_oauth_config(args: &[String]) -> Result<NotionOAuthCliConfig, CommandError> {
    let client_id = env_first(&["AFS_NOTION_OAUTH_CLIENT_ID", "NOTION_OAUTH_CLIENT_ID"])
        .ok_or_else(|| missing_oauth_config("AFS_NOTION_OAUTH_CLIENT_ID"))?;
    let client_secret = env_first(&[
        "AFS_NOTION_OAUTH_CLIENT_SECRET",
        "NOTION_OAUTH_CLIENT_SECRET",
    ])
    .ok_or_else(|| missing_oauth_config("AFS_NOTION_OAUTH_CLIENT_SECRET"))?;
    let redirect_uri = flag_value(args, "--redirect-uri")
        .map(str::to_string)
        .or_else(|| env_first(&["AFS_NOTION_OAUTH_REDIRECT_URI", "NOTION_OAUTH_REDIRECT_URI"]))
        .unwrap_or_else(|| "http://localhost:8757/oauth/notion/callback".to_string());

    local_redirect(&redirect_uri).map_err(|error| {
        CommandError::new("connect", error.code, error.message)
            .with_suggested_command("afs connect notion --token-stdin")
    })?;

    Ok(NotionOAuthCliConfig {
        client_id,
        client_secret,
        redirect_uri,
    })
}

fn notion_oauth_broker_config(args: &[String]) -> Result<NotionOAuthBrokerCliConfig, CommandError> {
    let broker_url = flag_value(args, "--broker-url")
        .map(str::to_string)
        .or_else(|| env_first(&["AFS_NOTION_OAUTH_BROKER_URL", "AFS_AUTH_BROKER_URL"]))
        .unwrap_or_else(|| DEFAULT_AFS_NOTION_OAUTH_BROKER_URL.to_string());
    let redirect_uri = flag_value(args, "--redirect-uri")
        .map(str::to_string)
        .or_else(|| env_first(&["AFS_NOTION_OAUTH_REDIRECT_URI", "NOTION_OAUTH_REDIRECT_URI"]))
        .unwrap_or_else(|| "http://localhost:8757/oauth/notion/callback".to_string());

    local_redirect(&redirect_uri).map_err(|error| {
        CommandError::new("connect", error.code, error.message)
            .with_suggested_command("afs connect notion --token-stdin")
    })?;

    Ok(NotionOAuthBrokerCliConfig {
        broker_url,
        redirect_uri,
    })
}

fn missing_oauth_config(name: &str) -> CommandError {
    CommandError::new(
        "connect",
        "missing_oauth_config",
        format!(
            "missing {name}; configure Notion OAuth env vars for --direct-oauth or use --token-stdin for a personal access token"
        ),
    )
    .with_suggested_command("afs connect notion --token-stdin")
}

fn run_local_notion_oauth(
    config: &NotionOAuthCliConfig,
    no_browser: bool,
    json: bool,
) -> Result<LocalOAuthAuthorization, CommandError> {
    let state = random_state();
    let authorize_url = notion_authorize_url(&config.client_id, &config.redirect_uri, &state);
    run_local_oauth_authorization(
        "Notion",
        &authorize_url,
        &config.redirect_uri,
        &state,
        no_browser,
        json,
    )
    .map_err(local_oauth_command_error)
}

fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| std::env::var(key).ok())
        .filter(|value| !value.is_empty())
}

fn local_oauth_command_error(error: LocalOAuthError) -> CommandError {
    let command_error = CommandError::new("connect", error.code, error.message);
    if command_error.code == "invalid_redirect_uri" {
        command_error.with_suggested_command("afs connect notion --token-stdin")
    } else {
        command_error
    }
}

fn warn_daemon_fallback(command: &str, reason: DaemonUnavailableReason) {
    if std::env::var("AFS_DAEMON_DISABLE").is_err() {
        match reason {
            DaemonUnavailableReason::TimedOut => eprintln!(
                "afsd did not respond within {}ms; executing {command} directly",
                daemon_mutating_request_timeout().as_millis()
            ),
            DaemonUnavailableReason::NotAvailable => eprintln!(
                "afsd not running; executing {command} directly (start afsd for background hydration)"
            ),
            DaemonUnavailableReason::Disabled => {}
        }
    }
}

fn pull_direct_fallback_error(
    reason: DaemonUnavailableReason,
    mount: Option<&MountConfig>,
) -> Option<CommandError> {
    match reason {
        DaemonUnavailableReason::TimedOut => Some(
            CommandError::new(
                "pull",
                "daemon_timeout",
                format!(
                    "afsd did not respond within {}ms after the pull request was submitted; refusing direct fallback to avoid racing daemon hydration",
                    daemon_mutating_request_timeout().as_millis()
                ),
            )
            .with_suggested_command("afs daemon restart"),
        ),
        DaemonUnavailableReason::NotAvailable
            if mount.is_some_and(|mount| mount.projection.uses_virtual_filesystem()) =>
        {
            Some(
                CommandError::new(
                    "pull",
                    "daemon_required",
                    format!(
                        "mount `{}` uses projection `{}`; pull for virtual projections must run through afsd so the provider cache stays serialized",
                        mount.expect("checked mount").mount_id.0,
                        mount.expect("checked mount").projection.as_str()
                    ),
                )
                .with_suggested_command("afs daemon restart"),
            )
        }
        DaemonUnavailableReason::Disabled | DaemonUnavailableReason::NotAvailable => None,
    }
}

fn resolve_mount_connection(
    store: &SqliteStateStore,
    args: &[String],
) -> Result<Option<ConnectionId>, CommandError> {
    let descriptor = source_descriptor("notion");
    if let Some(connection_id) = flag_value(args, "--connection") {
        let connection_id = ConnectionId::new(connection_id);
        let connection = store
            .get_connection(&connection_id)
            .map_err(|error| CommandError::new("mount", "store_error", error.to_string()))?
            .ok_or_else(|| {
                CommandError::new(
                    "mount",
                    "missing_connection",
                    format!("connection `{}` was not found", connection_id.0),
                )
                .with_optional_suggested_command(descriptor.connect_command())
            })?;
        if connection.status != "active" {
            return Err(CommandError::new(
                "mount",
                "connection_revoked",
                format!("connection `{}` is revoked", connection.connection_id.0),
            )
            .with_optional_suggested_command(descriptor.connect_command()));
        }
        validate_connection_profile(store, &connection, &descriptor)?;
        return Ok(Some(connection.connection_id));
    }

    let active = store
        .list_connections()
        .map_err(|error| CommandError::new("mount", "store_error", error.to_string()))?
        .into_iter()
        .filter(|connection| {
            connection.connector == descriptor.id() && connection.status == "active"
        })
        .collect::<Vec<_>>();
    for connection in &active {
        validate_connection_profile(store, connection, &descriptor)?;
    }
    match active.as_slice() {
        [connection] => Ok(Some(connection.connection_id.clone())),
        [] if descriptor
            .auth_env_var()
            .is_some_and(|env_var| std::env::var(env_var).is_ok()) =>
        {
            Ok(None)
        }
        [] => {
            let message = match descriptor.connect_command() {
                Some(command) => format!(
                    "missing {} connection; run `{command}`",
                    descriptor.display_name()
                ),
                None => format!("missing {} connection", descriptor.display_name()),
            };
            Err(CommandError::new("mount", "missing_connection", message)
                .with_optional_suggested_command(descriptor.connect_command()))
        }
        _ => Err(CommandError::new(
            "mount",
            "missing_connection",
            format!(
                "multiple {} connections exist; pass --connection <id>",
                descriptor.display_name()
            ),
        )),
    }
}

fn validate_connection_profile(
    store: &SqliteStateStore,
    connection: &ConnectionRecord,
    descriptor: &SourceDescriptor,
) -> Result<(), CommandError> {
    let Some(profile_id) = &connection.profile_id else {
        return Ok(());
    };
    let profile = store
        .get_connector_profile(profile_id)
        .map_err(|error| CommandError::new("mount", "store_error", error.to_string()))?
        .ok_or_else(|| {
            CommandError::new(
                "mount",
                "auth_profile_unavailable",
                format!("connector profile `{}` was not found", profile_id.0),
            )
            .with_optional_suggested_command(descriptor.connect_command())
        })?;
    if profile.status != "active" {
        return Err(CommandError::new(
            "mount",
            "auth_profile_unavailable",
            format!(
                "connector profile `{}` is {}",
                profile.profile_id.0, profile.status
            ),
        )
        .with_optional_suggested_command(descriptor.connect_command()));
    }
    if profile.connector != connection.connector || profile.auth_kind != connection.auth_kind {
        return Err(CommandError::new(
            "mount",
            "auth_profile_unavailable",
            format!(
                "connector profile `{}` does not match connection `{}`",
                profile.profile_id.0, connection.connection_id.0
            ),
        )
        .with_optional_suggested_command(descriptor.connect_command()));
    }
    Ok(())
}

enum DaemonReport<T> {
    Report(T),
    Unavailable(DaemonUnavailableReason),
    Error(DaemonCommandError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaemonUnavailableReason {
    Disabled,
    NotAvailable,
    TimedOut,
}

struct DaemonCommandError {
    code: String,
    message: String,
    exit_code: i32,
}

fn run_daemon_report<T>(state_root: &std::path::Path, request: &DaemonRequest) -> DaemonReport<T>
where
    T: DeserializeOwned,
{
    if std::env::var("AFS_DAEMON_DISABLE").is_ok() {
        return DaemonReport::Unavailable(DaemonUnavailableReason::Disabled);
    }

    let response =
        match send_request_with_timeout(state_root, request, daemon_request_timeout_for(request)) {
            Ok(response) => response,
            Err(DaemonClientError::NotAvailable(_)) => {
                return DaemonReport::Unavailable(DaemonUnavailableReason::NotAvailable);
            }
            Err(DaemonClientError::TimedOut(_)) => {
                return DaemonReport::Unavailable(DaemonUnavailableReason::TimedOut);
            }
            Err(error) => {
                return DaemonReport::Error(DaemonCommandError {
                    code: "daemon_error".to_string(),
                    message: error.message().to_string(),
                    exit_code: EXIT_INTERNAL,
                });
            }
        };

    if let Some(error) = response.error {
        let exit_code = daemon_error_exit_code(&error.code);
        return DaemonReport::Error(DaemonCommandError {
            code: error.code,
            message: error.message,
            exit_code,
        });
    }

    let Some(payload) = response.payload else {
        return DaemonReport::Error(DaemonCommandError {
            code: "daemon_protocol_error".to_string(),
            message: "daemon returned no payload".to_string(),
            exit_code: EXIT_INTERNAL,
        });
    };

    match serde_json::from_value(payload) {
        Ok(report) => DaemonReport::Report(report),
        Err(error) => DaemonReport::Error(DaemonCommandError {
            code: "daemon_protocol_error".to_string(),
            message: error.to_string(),
            exit_code: EXIT_INTERNAL,
        }),
    }
}

fn daemon_request_timeout() -> Duration {
    std::env::var("AFS_DAEMON_REQUEST_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_DAEMON_CONTROL_TIMEOUT)
}

fn daemon_mutating_request_timeout() -> Duration {
    std::env::var("AFS_DAEMON_REQUEST_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_DAEMON_MUTATING_TIMEOUT)
}

fn daemon_request_timeout_for(request: &DaemonRequest) -> Duration {
    match request {
        DaemonRequest::Pull { .. } | DaemonRequest::Push { .. } => {
            daemon_mutating_request_timeout()
        }
        _ => daemon_request_timeout(),
    }
}

fn notify_daemon_mounts_changed(state_root: &std::path::Path) {
    if std::env::var("AFS_DAEMON_DISABLE").is_ok() {
        return;
    }

    match send_request_with_timeout(
        state_root,
        &DaemonRequest::ReloadMounts,
        daemon_request_timeout(),
    ) {
        Ok(response) if response.ok => {}
        Ok(response) => {
            if let Some(error) = response.error {
                eprintln!(
                    "afs mount: daemon mount reload failed: {}: {}",
                    error.code, error.message
                );
            }
        }
        Err(DaemonClientError::NotAvailable(_) | DaemonClientError::TimedOut(_)) => {}
        Err(error) => eprintln!("afs mount: daemon mount reload failed: {}", error.message()),
    }
}

fn daemon_error_exit_code(code: &str) -> i32 {
    match code {
        "mount_not_found" | "entity_path_missing" => EXIT_USAGE,
        "validation_failed" => EXIT_VALIDATION,
        "not_implemented" => 5,
        "missing_connection"
        | "auth_required"
        | "connection_revoked"
        | "auth_profile_unavailable"
        | "credential_store_unavailable" => EXIT_INTERNAL,
        _ => EXIT_INTERNAL,
    }
}

fn command_error(json: bool, error: CommandError, exit_code: i32) -> i32 {
    if json {
        print_json(&error);
    } else {
        eprintln!("afs {}: {}", error.command, error.message);
        if let Some(suggested_command) = &error.suggested_command {
            eprintln!("hint: {suggested_command}");
        }
    }

    exit_code
}

fn connect_command_error(command: &'static str, json: bool, error: ConnectError) -> i32 {
    let exit_code = match &error {
        ConnectError::ConnectionNameRequired => EXIT_USAGE,
        ConnectError::ConnectionProbeFailed(_)
        | ConnectError::OAuthExchangeFailed(_)
        | ConnectError::CredentialEncode(_)
        | ConnectError::Credential(_)
        | ConnectError::Store(_) => EXIT_INTERNAL,
        ConnectError::ConnectionMissing(_) => EXIT_INTERNAL,
    };
    let mut payload = CommandError::new(command, error.code(), error.message());
    if let Some(suggested_command) = error.suggested_command() {
        payload = payload.with_suggested_command(suggested_command);
    }
    command_error(json, payload, exit_code)
}

fn connector_command_error(command: &'static str, json: bool, error: ConnectorResolveError) -> i32 {
    let exit_code = match error.code() {
        "mount_not_found" => EXIT_USAGE,
        "missing_connection"
        | "auth_required"
        | "connection_revoked"
        | "auth_profile_unavailable"
        | "credential_store_unavailable" => EXIT_INTERNAL,
        _ => EXIT_INTERNAL,
    };
    let mut payload = CommandError::new(command, error.code(), error.message());
    if let Some(suggested_command) = error.suggested_command() {
        payload = payload.with_suggested_command(suggested_command);
    }
    command_error(json, payload, exit_code)
}

fn history_command_error(command: &'static str, json: bool, error: HistoryError) -> i32 {
    let exit_code = history_error_exit_code(&error);
    command_error(
        json,
        CommandError::new(command, error.code(), error.message()),
        exit_code,
    )
}

fn daemon_command_error(json: bool, error: DaemonControlError) -> i32 {
    let exit_code = match error.code() {
        "usage" => EXIT_USAGE,
        _ => EXIT_INTERNAL,
    };
    command_error(
        json,
        CommandError::new("daemon", error.code(), error.message()),
        exit_code,
    )
}

fn mount_command_error(json: bool, error: MountError) -> i32 {
    command_error(
        json,
        CommandError::new("mount", error.code(), error.message()),
        EXIT_INTERNAL,
    )
}

fn pull_command_error(json: bool, error: PullError) -> i32 {
    let exit_code = match &error {
        PullError::MountNotFound(_)
        | PullError::Store(afs_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
        PullError::ReadFile { .. } | PullError::WriteFile { .. } => EXIT_INTERNAL,
        PullError::Store(_) | PullError::Connector(_) | PullError::CurrentDir(_) => EXIT_INTERNAL,
    };
    command_error(
        json,
        CommandError::new("pull", error.code(), error.message()),
        exit_code,
    )
}

fn status_command_error(json: bool, error: StatusError, state_root: PathBuf) -> i32 {
    let exit_code = match &error {
        StatusError::MountNotFound(_)
        | StatusError::Store(afs_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
        StatusError::CurrentDir(_) | StatusError::Store(_) => EXIT_INTERNAL,
    };
    let message = match &error {
        StatusError::MountNotFound(_) => {
            format!(
                "{} in state dir `{}`",
                error.message(),
                state_root.display()
            )
        }
        _ => error.message(),
    };
    command_error(
        json,
        CommandError::new("status", error.code(), message),
        exit_code,
    )
}

fn search_command_error(json: bool, error: SearchError) -> i32 {
    let exit_code = match &error {
        SearchError::EmptyQuery | SearchError::InvalidLimit => EXIT_USAGE,
        SearchError::Store(_) => EXIT_INTERNAL,
    };
    command_error(
        json,
        CommandError::new("search", error.code(), error.message()),
        exit_code,
    )
}

fn template_command_error(command: &'static str, json: bool, error: TemplatePackError) -> i32 {
    let exit_code = match &error {
        TemplatePackError::PackNotFound(_)
        | TemplatePackError::TemplateNotFound { .. }
        | TemplatePackError::ManifestMissing(_)
        | TemplatePackError::ManifestInvalid { .. }
        | TemplatePackError::InvalidPackId(_)
        | TemplatePackError::InvalidRelativePath(_)
        | TemplatePackError::TargetNotDirectory(_)
        | TemplatePackError::TargetNotEmpty(_)
        | TemplatePackError::FileExists(_)
        | TemplatePackError::SymlinkUnsupported(_) => EXIT_USAGE,
        TemplatePackError::Io(_) => EXIT_INTERNAL,
    };
    command_error(
        json,
        CommandError::new(command, error.code(), error.message()),
        exit_code,
    )
}

fn inspect_command_error(json: bool, error: InspectError) -> i32 {
    let exit_code = match &error {
        InspectError::MountNotFound(_)
        | InspectError::Store(afs_store::StoreError::EntityPathMissing { .. })
        | InspectError::UnsupportedEntity { .. } => EXIT_USAGE,
        InspectError::CurrentDir(_)
        | InspectError::ProjectionReadPath { .. }
        | InspectError::ReadFile { .. }
        | InspectError::Store(_)
        | InspectError::RemoteFetch(_) => EXIT_INTERNAL,
    };
    command_error(
        json,
        CommandError::new("inspect", error.code(), error.message()),
        exit_code,
    )
}

fn restore_command_error(json: bool, error: RestoreError) -> i32 {
    let exit_code = match &error {
        RestoreError::MountNotFound(_)
        | RestoreError::Store(afs_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
        RestoreError::ConflictedRequiresForce(_) => 4,
        RestoreError::CurrentDir(_)
        | RestoreError::Store(_)
        | RestoreError::UnsupportedEntity(_)
        | RestoreError::WriteFile { .. } => EXIT_INTERNAL,
    };
    command_error(
        json,
        CommandError::new("restore", error.code(), error.message()),
        exit_code,
    )
}

fn info_command_error(json: bool, error: InfoError, state_root: PathBuf) -> i32 {
    let exit_code = match &error {
        InfoError::MountNotFound(_)
        | InfoError::Store(afs_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
        InfoError::CurrentDir(_) | InfoError::Store(_) => EXIT_INTERNAL,
    };
    let message = match &error {
        InfoError::MountNotFound(_) => {
            format!(
                "{} in state dir `{}`",
                error.message(),
                state_root.display()
            )
        }
        _ => error.message(),
    };
    command_error(
        json,
        CommandError::new("info", error.code(), message),
        exit_code,
    )
}

fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            println!(
                "{{\"ok\":false,\"command\":\"internal\",\"code\":\"json_encode_failed\",\"message\":\"{}\"}}",
                escape_json_string(&error.to_string())
            );
        }
    }
}

fn diff_error_exit_code(error: &DiffError) -> i32 {
    match error {
        DiffError::MountNotFound(_) => EXIT_USAGE,
        DiffError::ReadFile { .. } => EXIT_INTERNAL,
        DiffError::Store(_) => EXIT_INTERNAL,
        DiffError::Prepare(_) => EXIT_INTERNAL,
    }
}

fn history_error_exit_code(error: &HistoryError) -> i32 {
    match error {
        HistoryError::MountNotFound(_)
        | HistoryError::JournalNotFound(_)
        | HistoryError::Store(afs_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
        HistoryError::Store(_) => EXIT_INTERNAL,
    }
}

fn afs_error_exit_code(error: &AfsError) -> i32 {
    match error {
        AfsError::Validation(_) => EXIT_VALIDATION,
        AfsError::NotImplemented(_) => 5,
        _ => EXIT_INTERNAL,
    }
}

fn afs_error_code(error: &AfsError) -> &'static str {
    match error {
        AfsError::Validation(_) => "validation_failed",
        AfsError::Conflict(_) => "conflict",
        AfsError::Guardrail(_) => "guardrail",
        AfsError::InvalidState(_) => "invalid_state",
        AfsError::Unsupported(_) => "unsupported",
        AfsError::NotImplemented(_) => "not_implemented",
        AfsError::Io(_) => "io_error",
    }
}

fn diff_report_exit_code(report: &crate::diff::DiffReport) -> i32 {
    if report.ok {
        EXIT_SUCCESS
    } else {
        EXIT_VALIDATION
    }
}

fn pull_report_exit_code(report: &PullReport) -> i32 {
    if report.ok {
        EXIT_SUCCESS
    } else {
        EXIT_VALIDATION
    }
}

fn first_positional(args: &[String]) -> Option<&str> {
    nth_positional(args, 0)
}

fn positional_args(args: &[String]) -> Vec<String> {
    let mut values = Vec::new();
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
        values.push(arg.clone());
    }

    values
}

fn nth_positional(args: &[String], index: usize) -> Option<&str> {
    let mut seen = 0;
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
        if seen == index {
            return Some(arg.as_str());
        }
        seen += 1;
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

fn projection_mode(args: &[String]) -> Result<ProjectionMode, String> {
    projection_mode_for_target(args, std::env::consts::OS)
}

fn projection_mode_for_target(args: &[String], target_os: &str) -> Result<ProjectionMode, String> {
    afs_platform::mount_cli_capabilities_for_target(target_os)
        .projection_from_cli_value(flag_value(args, "--projection"))
        .map_err(|error| error.message())
}

fn mount_usage() -> String {
    let descriptor = source_descriptor("notion");
    format!(
        "usage: afs mount {} <path> (--workspace|--root-page <page-id>) [--connection <id>] [--mount-id <id>] [--projection {}] [--read-only] [--json]",
        descriptor.id(),
        projection_usage_options_for_target(std::env::consts::OS)
    )
}

fn projection_usage_options_for_target(target_os: &str) -> String {
    afs_platform::mount_cli_capabilities_for_target(target_os).projection_usage_options()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VirtualProjectionRegistration {
    MacosFileProvider,
    LinuxFuse,
    WindowsCloudFiles,
}

impl VirtualProjectionRegistration {
    fn projection(self) -> ProjectionMode {
        match self {
            Self::MacosFileProvider => ProjectionMode::MacosFileProvider,
            Self::LinuxFuse => ProjectionMode::LinuxFuse,
            Self::WindowsCloudFiles => ProjectionMode::WindowsCloudFiles,
        }
    }

    fn projection_cli_value(self) -> &'static str {
        afs_platform::capabilities::projection_cli_value(&self.projection())
    }
}

fn validate_virtual_projection_registration(
    mount: &MountConfig,
    target_os: &str,
) -> Result<VirtualProjectionRegistration, CommandError> {
    let Some(registration) = virtual_projection_registration_for_target(target_os) else {
        return Err(CommandError::new(
            "file-provider",
            "unsupported_platform",
            format!("no virtual filesystem registration is implemented for {target_os}"),
        ));
    };
    let required_projection = registration.projection();

    if mount.projection == required_projection {
        return Ok(registration);
    }

    Err(CommandError::new(
        "file-provider",
        "wrong_projection",
        format!(
            "mount `{}` uses projection `{}`; remount with --projection {}",
            mount.mount_id.0,
            mount.projection.as_str(),
            registration.projection_cli_value()
        ),
    ))
}

fn virtual_projection_registration_for_target(
    target_os: &str,
) -> Option<VirtualProjectionRegistration> {
    match afs_platform::mount_cli_capabilities_for_target(target_os).virtual_registration {
        Some(ProjectionMode::MacosFileProvider) => {
            Some(VirtualProjectionRegistration::MacosFileProvider)
        }
        Some(ProjectionMode::LinuxFuse) => Some(VirtualProjectionRegistration::LinuxFuse),
        Some(ProjectionMode::WindowsCloudFiles) => {
            Some(VirtualProjectionRegistration::WindowsCloudFiles)
        }
        _ => None,
    }
}

fn auto_registration_for_mounted_projection(
    projection: ProjectionMode,
    target_os: &str,
    daemon_disabled: bool,
) -> Option<VirtualProjectionRegistration> {
    if daemon_disabled {
        return None;
    }

    match (projection, target_os) {
        (ProjectionMode::LinuxFuse, "linux") => Some(VirtualProjectionRegistration::LinuxFuse),
        (ProjectionMode::WindowsCloudFiles, "windows") => {
            Some(VirtualProjectionRegistration::WindowsCloudFiles)
        }
        _ => None,
    }
}

fn auto_register_mounted_projection(
    state_root: &Path,
    store: &SqliteStateStore,
    mount_id: &MountId,
) -> Result<(), CommandError> {
    let mount = store
        .get_mount(mount_id)
        .map_err(|error| CommandError::new("mount", "store_error", error.to_string()))?
        .ok_or_else(|| {
            CommandError::new(
                "mount",
                "mount_not_found",
                format!(
                    "mount `{}` was not found after mount registration",
                    mount_id.0
                ),
            )
        })?;
    let Some(registration) = auto_registration_for_mounted_projection(
        mount.projection.clone(),
        std::env::consts::OS,
        std::env::var_os("AFS_DAEMON_DISABLE").is_some(),
    ) else {
        return Ok(());
    };

    match registration {
        VirtualProjectionRegistration::LinuxFuse => {
            file_provider_helper::register_linux_fuse_mount(state_root, &mount)
                .map(|_| ())
                .map_err(|error| {
                    CommandError::new(
                        "mount",
                        error.code(),
                        format!(
                            "mounted `{}` but Linux FUSE registration failed: {}",
                            mount.mount_id.0,
                            error.message()
                        ),
                    )
                    .with_suggested_command(format!(
                        "afs file-provider register {}",
                        mount.root.display()
                    ))
                })
        }
        VirtualProjectionRegistration::MacosFileProvider => Ok(()),
        VirtualProjectionRegistration::WindowsCloudFiles => {
            let display_name = file_provider_display_name(&mount);
            file_provider_helper::register_windows_cloud_files_sync_root(
                state_root,
                &mount,
                &display_name,
            )
            .map(|_| ())
            .map_err(|error| {
                CommandError::new(
                    "mount",
                    error.code(),
                    format!(
                        "mounted `{}` but Windows Cloud Files registration failed: {}",
                        mount.mount_id.0,
                        error.message()
                    ),
                )
                .with_suggested_command(format!(
                    "afs file-provider register {}",
                    mount.root.display()
                ))
            })
        }
    }
}

fn takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "--root-page"
            | "--mount-id"
            | "--connection"
            | "--name"
            | "--projection"
            | "--helper"
            | "--display-name"
            | "--redirect-uri"
            | "--broker-url"
            | "--connector"
            | "--limit"
    )
}

fn default_state_root() -> PathBuf {
    afs_platform::default_state_root()
}

fn reconcile_projection_changes(
    command: &'static str,
    store: &mut SqliteStateStore,
    state_root: &Path,
    target: Option<&Path>,
) -> Result<(), CommandError> {
    daemon_file_provider::reconcile_macos_file_provider_projection(store, state_root, target)
        .map(|_| ())
        .map_err(|error| {
            CommandError::new(
                command,
                "projection_reconcile_failed",
                format!("failed to reconcile macOS File Provider changes: {error}"),
            )
        })
}

fn reconcile_projection_changes_best_effort(
    command: &'static str,
    store: &mut SqliteStateStore,
    state_root: &Path,
    target: Option<&Path>,
) {
    if let Err(error) =
        daemon_file_provider::reconcile_macos_file_provider_projection(store, state_root, target)
    {
        eprintln!("afs {command}: skipped macOS File Provider reconciliation: {error}");
    }
}

fn escape_json_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[derive(Debug, Serialize)]
struct CommandError {
    ok: bool,
    command: &'static str,
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    suggested_command: Option<String>,
}

#[derive(Serialize)]
struct FileProviderCommandReport {
    ok: bool,
    command: &'static str,
    action: String,
    mount_id: Option<String>,
    helper: String,
    helper_report: Value,
}

impl CommandError {
    fn new(command: &'static str, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            command,
            code: code.into(),
            message: message.into(),
            suggested_command: None,
        }
    }

    fn with_suggested_command(mut self, suggested_command: impl Into<String>) -> Self {
        self.suggested_command = Some(suggested_command.into());
        self
    }

    fn with_optional_suggested_command(mut self, suggested_command: Option<&str>) -> Self {
        self.suggested_command = suggested_command.map(str::to_string);
        self
    }
}

fn print_help() {
    let _ = Cli::command().print_help();
    println!();
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::Path;

    use clap::Parser;
    use clap::error::ErrorKind;

    use afs_core::model::MountId;
    use afs_store::{MountConfig, ProjectionMode};

    use crate::diff::{DiffReport, GuardrailOutput};
    use crate::local_oauth::{local_redirect, notion_authorize_url, parse_oauth_callback};
    use crate::push::PushReport;
    use crate::search::{SearchOptions, SearchReport};

    use super::{
        Cli, DaemonUnavailableReason, EXIT_SUCCESS, EXIT_VALIDATION, VirtualProjectionRegistration,
        absolute_command_path, auto_registration_for_mounted_projection, diff_report_exit_code,
        legacy_args_for_command, notion_oauth_broker_config, projection_mode_for_target,
        projection_usage_options_for_target, prompt_for_push_confirmation,
        pull_direct_fallback_error, should_prompt_for_push_confirmation,
        should_refresh_notion_url_search, spinner_config_for_command, spinner_enabled,
        validate_virtual_projection_registration,
    };

    #[test]
    fn clap_help_is_available_for_commands_and_nested_subcommands() {
        let cases = vec![
            (
                vec!["--help"],
                vec!["Usage: afs", "Commands:", "push", "file-provider"],
            ),
            (
                vec!["connect", "--help"],
                vec!["Usage: afs connect", "Commands:", "notion", "--json"],
            ),
            (
                vec!["connect", "notion", "--help"],
                vec![
                    "Usage: afs connect notion",
                    "Connect a Notion workspace",
                    "--token-stdin",
                    "--direct-oauth",
                ],
            ),
            (
                vec!["connections", "--help"],
                vec!["Usage: afs connections", "List saved source", "--json"],
            ),
            (
                vec!["profiles", "--help"],
                vec!["Usage: afs profiles", "List connector profiles", "--json"],
            ),
            (
                vec!["connection", "--help"],
                vec!["Usage: afs connection", "Commands:", "show", "--json"],
            ),
            (
                vec!["connection", "show", "--help"],
                vec![
                    "Usage: afs connection show",
                    "Show connection details",
                    "id",
                    "--json",
                ],
            ),
            (
                vec!["disconnect", "--help"],
                vec!["Usage: afs disconnect", "Disconnect", "id", "--json"],
            ),
            (
                vec!["daemon", "--help"],
                vec!["Usage: afs daemon", "Commands:", "start", "restart"],
            ),
            (
                vec!["daemon", "start", "--help"],
                vec![
                    "Usage: afs daemon start",
                    "Start the daemon",
                    "--session",
                    "--afsd-bin",
                ],
            ),
            (
                vec!["daemon", "stop", "--help"],
                vec!["Usage: afs daemon stop", "Stop the daemon", "--tcp-addr"],
            ),
            (
                vec!["daemon", "status", "--help"],
                vec![
                    "Usage: afs daemon status",
                    "Show daemon status",
                    "--tcp-addr",
                ],
            ),
            (
                vec!["daemon", "reload", "--help"],
                vec!["Usage: afs daemon reload", "Reload daemon", "--tcp-addr"],
            ),
            (
                vec!["daemon", "restart", "--help"],
                vec![
                    "Usage: afs daemon restart",
                    "Restart the daemon",
                    "--tcp-addr",
                ],
            ),
            (
                vec!["mount", "--help"],
                vec!["Usage: afs mount", "Commands:", "notion", "--json"],
            ),
            (
                vec!["mount", "notion", "--help"],
                vec![
                    "Usage: afs mount notion",
                    "Mount Notion content",
                    "--workspace",
                    "--root-page",
                ],
            ),
            (
                vec!["info", "--help"],
                vec!["Usage: afs info", "Show source", "path", "--json"],
            ),
            (
                vec!["status", "--help"],
                vec![
                    "Usage: afs status",
                    "Show local sync state",
                    "path",
                    "--json",
                ],
            ),
            (
                vec!["doctor", "--help"],
                vec!["Usage: afs doctor", "Run read-only diagnostics", "--json"],
            ),
            (
                vec!["search", "--help"],
                vec![
                    "Usage: afs search",
                    "Search local mount metadata",
                    "--connector",
                    "--limit",
                ],
            ),
            (
                vec!["templates", "--help"],
                vec![
                    "Usage: afs templates",
                    "Commands:",
                    "list",
                    "validate",
                    "new",
                    "apply",
                ],
            ),
            (
                vec!["templates", "new", "--help"],
                vec![
                    "Usage: afs templates new",
                    "Create a local workspace",
                    "--force",
                ],
            ),
            (
                vec!["templates", "apply", "--help"],
                vec!["Usage: afs templates apply", "--to", "--title", "--force"],
            ),
            (
                vec!["pull", "--help"],
                vec!["Usage: afs pull", "Pull remote content", "path", "--json"],
            ),
            (
                vec!["push", "--help"],
                vec![
                    "Usage: afs push",
                    "Push local changes",
                    "--yes",
                    "--confirm",
                ],
            ),
            (
                vec!["diff", "--help"],
                vec!["Usage: afs diff", "Preview the push plan", "path", "--json"],
            ),
            (
                vec!["undo", "--help"],
                vec![
                    "Usage: afs undo",
                    "Undo a reconciled push",
                    "push-id",
                    "--json",
                ],
            ),
            (
                vec!["log", "--help"],
                vec!["Usage: afs log", "List push journal", "path", "--json"],
            ),
            (
                vec!["restore", "--help"],
                vec![
                    "Usage: afs restore",
                    "Restore a local file",
                    "--force",
                    "--json",
                ],
            ),
            (
                vec!["config", "--help"],
                vec!["Usage: afs config", "Configuration commands", "--json"],
            ),
            (
                vec!["file-provider", "--help"],
                vec![
                    "Usage: afs file-provider",
                    "Commands:",
                    "register",
                    "start",
                    "status",
                    "reset",
                ],
            ),
            (
                vec!["file-provider", "register", "--help"],
                vec![
                    "Usage: afs file-provider register",
                    "Register a virtual filesystem",
                    "mount-id-or-path",
                    "--json",
                ],
            ),
            (
                vec!["file-provider", "start", "--help"],
                vec![
                    "Usage: afs file-provider start",
                    "Start the background provider",
                    "mount-id-or-path",
                ],
            ),
            (
                vec!["file-provider", "stop", "--help"],
                vec![
                    "Usage: afs file-provider stop",
                    "Stop the background provider",
                    "mount-id-or-path",
                ],
            ),
            (
                vec!["file-provider", "status", "--help"],
                vec![
                    "Usage: afs file-provider status",
                    "Show provider runtime status",
                    "mount-id-or-path",
                ],
            ),
            (
                vec!["file-provider", "restart", "--help"],
                vec![
                    "Usage: afs file-provider restart",
                    "Restart the background provider",
                    "mount-id-or-path",
                ],
            ),
            (
                vec!["file-provider", "open", "--help"],
                vec![
                    "Usage: afs file-provider open",
                    "Open a registered virtual filesystem",
                    "mount-id-or-path",
                ],
            ),
            (
                vec!["file-provider", "unregister", "--help"],
                vec![
                    "Usage: afs file-provider unregister",
                    "Unregister a virtual filesystem",
                    "mount-id-or-path",
                ],
            ),
            (
                vec!["file-provider", "list", "--help"],
                vec![
                    "Usage: afs file-provider list",
                    "List registered file provider",
                ],
            ),
            (
                vec!["file-provider", "reset", "--help"],
                vec!["Usage: afs file-provider reset", "Reset file provider"],
            ),
        ];

        for (args, expected) in cases {
            let help = clap_help(args);
            for needle in expected {
                assert!(
                    help.contains(needle),
                    "expected help to contain `{needle}`:\n{help}"
                );
            }
        }
    }

    #[test]
    fn clap_json_help_still_displays_text_help() {
        let help = clap_help(vec!["push", "Roadmap.md", "--json", "--help"]);

        assert!(help.contains("Usage: afs push"));
        assert!(help.contains("Push local changes"));
        assert!(!help.trim_start().starts_with('{'));
    }

    #[test]
    fn clap_parsed_commands_convert_to_legacy_args_for_execution() {
        let cli = parse_cli(["--json", "push", "Roadmap.md", "--yes", "--confirm"]);
        assert!(cli.json);
        assert_eq!(
            legacy_args_for_command(cli.command.as_ref().expect("command")),
            vec!["push", "Roadmap.md", "--yes", "--confirm"]
        );

        let cli = parse_cli([
            "daemon",
            "start",
            "--session",
            "--state-dir",
            "/tmp/afs-state",
            "--include-env",
            "NOTION_TOKEN",
        ]);
        assert_eq!(
            legacy_args_for_command(cli.command.as_ref().expect("command")),
            vec![
                "daemon",
                "start",
                "--session",
                "--state-dir",
                "/tmp/afs-state",
                "--include-env",
                "NOTION_TOKEN"
            ]
        );

        let cli = parse_cli([
            "search",
            "initial",
            "idea",
            "--connector",
            "notion",
            "--limit",
            "5",
        ]);
        assert_eq!(
            legacy_args_for_command(cli.command.as_ref().expect("command")),
            vec![
                "search",
                "initial",
                "idea",
                "--connector",
                "notion",
                "--limit",
                "5"
            ]
        );

        let cli = parse_cli([
            "templates",
            "new",
            "founder-proof-of-work",
            "/tmp/founder",
            "--force",
        ]);
        assert_eq!(
            legacy_args_for_command(cli.command.as_ref().expect("command")),
            vec![
                "templates",
                "new",
                "founder-proof-of-work",
                "/tmp/founder",
                "--force"
            ]
        );

        let cli = parse_cli(["file-provider", "restart", "notion-main"]);
        assert_eq!(
            legacy_args_for_command(cli.command.as_ref().expect("command")),
            vec!["file-provider", "restart", "notion-main"]
        );

        let cli = parse_cli(["doctor"]);
        assert_eq!(
            legacy_args_for_command(cli.command.as_ref().expect("command")),
            vec!["doctor"]
        );
    }

    #[test]
    fn clean_diff_report_exits_successfully() {
        assert_eq!(diff_report_exit_code(&report(true)), EXIT_SUCCESS);
    }

    #[test]
    fn validation_diff_report_exits_with_validation_code() {
        assert_eq!(diff_report_exit_code(&report(false)), EXIT_VALIDATION);
    }

    #[test]
    fn push_report_exit_codes_track_gate_states() {
        assert_eq!(
            crate::push::push_report_exit_code(&push_report("noop")),
            EXIT_SUCCESS
        );
        assert_eq!(
            crate::push::push_report_exit_code(&push_report("fix_validation")),
            EXIT_VALIDATION
        );
        assert_eq!(
            crate::push::push_report_exit_code(&push_report("confirm_plan")),
            4
        );
        assert_eq!(
            crate::push::push_report_exit_code(&push_report("apply_not_implemented")),
            5
        );
    }

    #[test]
    fn push_confirmation_prompt_accepts_yes_and_rejects_no() {
        let mut yes_output = Vec::new();
        let yes = prompt_for_push_confirmation(&mut Cursor::new(b"y\n"), &mut yes_output)
            .expect("yes prompt");
        assert!(yes);
        assert_eq!(
            String::from_utf8(yes_output).expect("yes utf8"),
            "Proceed with push? [y/N] "
        );

        let mut no_output = Vec::new();
        let no = prompt_for_push_confirmation(&mut Cursor::new(b"n\n"), &mut no_output)
            .expect("no prompt");
        assert!(!no);
    }

    #[test]
    fn push_confirmation_prompt_is_only_for_interactive_safe_plans() {
        let options = crate::push::PushOptions {
            assume_yes: false,
            confirm_dangerous: false,
        };

        assert!(should_prompt_for_push_confirmation(
            &push_report("confirm_plan"),
            &options,
            false,
            true
        ));
        assert!(!should_prompt_for_push_confirmation(
            &push_report("confirm_plan"),
            &options,
            true,
            true
        ));
        assert!(!should_prompt_for_push_confirmation(
            &push_report("confirm_plan"),
            &options,
            false,
            false
        ));
        assert!(!should_prompt_for_push_confirmation(
            &push_report("confirm_dangerous_plan"),
            &options,
            false,
            true
        ));
    }

    #[test]
    fn spinner_is_only_enabled_for_human_terminal_output() {
        assert!(spinner_enabled(false, true));
        assert!(!spinner_enabled(true, true));
        assert!(!spinner_enabled(false, false));
        assert!(!spinner_enabled(true, false));
    }

    #[test]
    fn spinner_config_uses_command_specific_loading_labels() {
        let pull = spinner_config_for_command("pull", "Roadmap.md", false, true);
        assert!(pull.enabled);
        assert_eq!(pull.label, "pulling Roadmap.md");

        let push = spinner_config_for_command("push", "Roadmap.md", false, true);
        assert!(push.enabled);
        assert_eq!(push.label, "pushing Roadmap.md");
    }

    #[test]
    fn spinner_config_is_disabled_for_json() {
        let config = spinner_config_for_command("pull", "Roadmap.md", true, true);

        assert!(!config.enabled);
        assert_eq!(config.label, "pulling Roadmap.md");
    }

    #[test]
    fn notion_url_search_miss_triggers_metadata_refresh() {
        let options = SearchOptions::new(
            "https://app.notion.com/p/codeflash/Email-Outreach-1fa3ac0ebb8880e580cbcfd7e54f9be2",
        );
        let report = empty_search_report(&options);

        assert!(should_refresh_notion_url_search(&options, &report));
    }

    #[test]
    fn notion_url_search_refresh_skips_non_notion_or_existing_matches() {
        let mut options = SearchOptions::new(
            "https://app.notion.com/p/codeflash/Email-Outreach-1fa3ac0ebb8880e580cbcfd7e54f9be2",
        );
        options.connector = Some("linear".to_string());
        let report = empty_search_report(&options);

        assert!(!should_refresh_notion_url_search(&options, &report));

        let options = SearchOptions::new("Email Outreach");
        let report = empty_search_report(&options);

        assert!(!should_refresh_notion_url_search(&options, &report));
    }

    #[test]
    fn projection_mode_accepts_only_linux_virtual_projection_on_linux() {
        let args = vec!["--projection".to_string(), "linux-fuse".to_string()];

        assert_eq!(
            projection_mode_for_target(&args, "linux").expect("linux fuse projection"),
            ProjectionMode::LinuxFuse
        );
        assert!(
            projection_mode_for_target(&args, "macos")
                .expect_err("linux fuse rejected on macos")
                .contains("only supported on Linux")
        );
        assert_eq!(
            projection_usage_options_for_target("linux"),
            "plain-files|linux-fuse"
        );
    }

    #[test]
    fn projection_mode_accepts_only_macos_virtual_projection_on_macos() {
        let args = vec![
            "--projection".to_string(),
            "macos-file-provider".to_string(),
        ];

        assert_eq!(
            projection_mode_for_target(&args, "macos").expect("macos file provider projection"),
            ProjectionMode::MacosFileProvider
        );
        assert!(
            projection_mode_for_target(&args, "linux")
                .expect_err("macos file provider rejected on linux")
                .contains("only supported on macOS")
        );
        assert_eq!(
            projection_usage_options_for_target("macos"),
            "plain-files|macos-file-provider"
        );
    }

    #[test]
    fn projection_mode_defaults_to_plain_files_on_every_platform() {
        let args = Vec::new();

        assert_eq!(
            projection_mode_for_target(&args, "windows").expect("plain files default"),
            ProjectionMode::PlainFiles
        );
        assert_eq!(
            projection_mode_for_target(
                &[
                    "--projection".to_string(),
                    "windows-cloud-files".to_string()
                ],
                "windows"
            )
            .expect("windows cloud files projection"),
            ProjectionMode::WindowsCloudFiles
        );
        assert_eq!(
            projection_usage_options_for_target("windows"),
            "plain-files|windows-cloud-files"
        );
    }

    #[test]
    fn virtual_projection_registration_is_platform_specific() {
        let macos_mount =
            MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/afs/notion")
                .projection(ProjectionMode::MacosFileProvider);
        let linux_mount =
            MountConfig::new(MountId::new("notion-linux"), "notion", "/tmp/afs/linux")
                .projection(ProjectionMode::LinuxFuse);
        let windows_mount = MountConfig::new(
            MountId::new("notion-windows"),
            "notion",
            r"C:\Users\Ada\AFS",
        )
        .projection(ProjectionMode::WindowsCloudFiles);

        assert_eq!(
            validate_virtual_projection_registration(&macos_mount, "macos")
                .expect("macos file provider mount is valid"),
            VirtualProjectionRegistration::MacosFileProvider
        );
        assert_eq!(
            validate_virtual_projection_registration(&linux_mount, "linux")
                .expect("linux fuse mount is valid"),
            VirtualProjectionRegistration::LinuxFuse
        );
        assert_eq!(
            validate_virtual_projection_registration(&windows_mount, "windows")
                .expect("windows cloud files mount is valid"),
            VirtualProjectionRegistration::WindowsCloudFiles
        );

        let wrong_projection = validate_virtual_projection_registration(&linux_mount, "macos")
            .expect_err("linux fuse mount is not a macos file provider domain");
        assert_eq!(wrong_projection.code, "wrong_projection");
        assert!(
            wrong_projection
                .message
                .contains("--projection macos-file-provider")
        );

        let wrong_projection = validate_virtual_projection_registration(&macos_mount, "linux")
            .expect_err("macos file provider mount is not a linux fuse mount");
        assert_eq!(wrong_projection.code, "wrong_projection");
        assert!(wrong_projection.message.contains("--projection linux-fuse"));

        let wrong_projection = validate_virtual_projection_registration(&macos_mount, "windows")
            .expect_err("macos file provider mount is not a windows cloud files sync root");
        assert_eq!(wrong_projection.code, "wrong_projection");
        assert!(
            wrong_projection
                .message
                .contains("--projection windows-cloud-files")
        );
    }

    #[test]
    fn mount_auto_registration_runs_for_linux_fuse_on_linux_only() {
        assert_eq!(
            auto_registration_for_mounted_projection(ProjectionMode::LinuxFuse, "linux", false),
            Some(VirtualProjectionRegistration::LinuxFuse)
        );
        assert_eq!(
            auto_registration_for_mounted_projection(
                ProjectionMode::WindowsCloudFiles,
                "windows",
                false
            ),
            Some(VirtualProjectionRegistration::WindowsCloudFiles)
        );
        assert_eq!(
            auto_registration_for_mounted_projection(ProjectionMode::PlainFiles, "linux", false),
            None
        );
        assert_eq!(
            auto_registration_for_mounted_projection(ProjectionMode::LinuxFuse, "macos", false),
            None
        );
    }

    #[test]
    fn mount_auto_registration_skips_when_daemon_is_disabled() {
        assert_eq!(
            auto_registration_for_mounted_projection(ProjectionMode::LinuxFuse, "linux", true),
            None
        );
    }

    #[test]
    fn command_paths_absolutize_relative_state_roots() {
        let path = absolute_command_path(Path::new(".afs"));

        assert!(path.is_absolute());
        assert!(path.ends_with(".afs"));
    }

    #[test]
    fn pull_direct_fallback_refuses_timeout_and_virtual_mount_without_daemon() {
        let virtual_mount =
            MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/afs/notion")
                .projection(ProjectionMode::LinuxFuse);
        let plain_mount = MountConfig::new(MountId::new("plain"), "notion", "/tmp/afs/plain")
            .projection(ProjectionMode::PlainFiles);

        let timeout = pull_direct_fallback_error(DaemonUnavailableReason::TimedOut, None)
            .expect("timed out daemon pull blocks fallback");
        assert_eq!(timeout.code, "daemon_timeout");
        assert!(
            timeout
                .message
                .contains("refusing direct fallback to avoid racing daemon hydration")
        );

        let virtual_without_daemon =
            pull_direct_fallback_error(DaemonUnavailableReason::NotAvailable, Some(&virtual_mount))
                .expect("virtual projection requires daemon");
        assert_eq!(virtual_without_daemon.code, "daemon_required");
        assert!(virtual_without_daemon.message.contains("linux_fuse"));

        assert!(
            pull_direct_fallback_error(DaemonUnavailableReason::NotAvailable, Some(&plain_mount))
                .is_none()
        );
        assert!(
            pull_direct_fallback_error(DaemonUnavailableReason::Disabled, Some(&virtual_mount))
                .is_none()
        );
    }

    #[test]
    fn local_redirect_defaults_to_localhost_callback_uri() {
        let redirect =
            local_redirect("http://localhost:8757/oauth/notion/callback").expect("redirect");

        assert_eq!(redirect.bind_addr, "localhost:8757");
        assert_eq!(redirect.callback_path, "/oauth/notion/callback");
    }

    #[test]
    fn local_redirect_accepts_explicit_loopback_ip_callback_uri() {
        let redirect =
            local_redirect("http://127.0.0.1:8757/oauth/notion/callback").expect("redirect");

        assert_eq!(redirect.bind_addr, "127.0.0.1:8757");
        assert_eq!(redirect.callback_path, "/oauth/notion/callback");
    }

    #[test]
    fn oauth_callback_requires_matching_state() {
        let request = "GET /oauth/notion/callback?code=abc123&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n";

        let authorization =
            parse_oauth_callback(request, "/oauth/notion/callback", "expected").expect("callback");

        assert_eq!(authorization.code, "abc123");
        assert!(
            parse_oauth_callback(request, "/oauth/notion/callback", "other")
                .expect_err("state mismatch")
                .code
                .contains("oauth_state_mismatch")
        );
    }

    #[test]
    fn notion_authorize_url_encodes_redirect_and_state() {
        let url = notion_authorize_url(
            "client id",
            "http://localhost:8757/oauth/notion/callback",
            "state+value",
        );

        assert!(url.contains("client_id=client%20id"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("owner=user"));
        assert!(
            url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8757%2Foauth%2Fnotion%2Fcallback")
        );
        assert!(url.contains("state=state%2Bvalue"));
    }

    #[test]
    fn notion_oauth_broker_config_accepts_explicit_broker_url() {
        let args = vec![
            "notion".to_string(),
            "--broker-url".to_string(),
            "https://auth.example.test".to_string(),
            "--redirect-uri".to_string(),
            "http://localhost:8757/oauth/notion/callback".to_string(),
        ];

        let config = notion_oauth_broker_config(&args).expect("broker config");

        assert_eq!(config.broker_url, "https://auth.example.test");
        assert_eq!(
            config.redirect_uri,
            "http://localhost:8757/oauth/notion/callback"
        );
    }

    fn report(ok: bool) -> DiffReport {
        DiffReport {
            ok,
            command: "diff",
            path: "Roadmap.md".to_string(),
            mount_id: "notion-main".to_string(),
            entity_id: "page-1".to_string(),
            validation: Vec::new(),
            plan: None,
            guardrail: GuardrailOutput {
                decision: "proceed".to_string(),
                reasons: Vec::new(),
            },
            action: if ok { "noop" } else { "fix_validation" }.to_string(),
            unsupported: Vec::new(),
            message: None,
            suggested_fix: None,
            completed_stages: Vec::new(),
        }
    }

    fn push_report(action: &str) -> PushReport {
        PushReport {
            ok: action == "noop",
            command: "push",
            via: "cli".to_string(),
            path: "Roadmap.md".to_string(),
            mount_id: "notion-main".to_string(),
            entity_id: "page-1".to_string(),
            validation: Vec::new(),
            plan: None,
            guardrail: GuardrailOutput {
                decision: "proceed".to_string(),
                reasons: Vec::new(),
            },
            action: action.to_string(),
            pipeline_action: action.to_string(),
            push_id: None,
            journal_status: None,
            changed_remote_ids: Vec::new(),
            reconciled_remote_ids: Vec::new(),
            apply_effect_count: 0,
            completed_stages: Vec::new(),
            message: None,
            unsupported: Vec::new(),
            suggested_fix: None,
        }
    }

    fn empty_search_report(options: &SearchOptions) -> SearchReport {
        SearchReport {
            ok: true,
            command: "search",
            query: options.query.clone(),
            connector: options.connector.clone(),
            count: 0,
            results: Vec::new(),
        }
    }

    fn clap_help(args: Vec<&str>) -> String {
        let error = Cli::try_parse_from(argv(args)).expect_err("help exits through clap error");
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        error.to_string()
    }

    fn parse_cli<const N: usize>(args: [&str; N]) -> Cli {
        Cli::try_parse_from(argv(args)).expect("parse cli")
    }

    fn argv<I, S>(args: I) -> Vec<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        std::iter::once("afs".to_string())
            .chain(args.into_iter().map(|arg| arg.as_ref().to_string()))
            .collect()
    }
}
