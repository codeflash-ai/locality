use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command as ProcessCommand;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use clap::{Args, CommandFactory, Parser, Subcommand};
use locality_connector::ConnectorUndoApplier;
use locality_connector::oauth_broker::OAuthBrokerStart;
use locality_core::LocalityError;
use locality_core::journal::{JournalStatus, PushId};
use locality_core::model::{EntityKind, MountId, RemoteId};
use locality_core::path_projection::{
    page_container_path, page_document_path, page_listing_parent_path,
};
use locality_google_docs::{
    DEFAULT_GOOGLE_DOCS_OAUTH_BROKER_URL, DEFAULT_GOOGLE_DOCS_OAUTH_REDIRECT_URI,
    GOOGLE_DOCS_CONNECTOR_ID, HttpGoogleDocsOAuthBrokerClient,
};
use locality_notion::oauth::{
    DEFAULT_LOCALITY_NOTION_OAUTH_BROKER_URL, DEFAULT_NOTION_OAUTH_AUTHORIZE_URL,
    HttpNotionOAuthBrokerClient, HttpNotionOAuthClient, NotionOAuthBrokerStart,
};
use locality_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, ConnectorProfileRepository,
    EntityRepository, JournalRepository, MountConfig, MountRepository, ProjectionMode,
    SqliteStateStore, open_credential_store,
};
use localityd::execution::PushJobReport;
use localityd::file_provider as daemon_file_provider;
use localityd::google_docs::resolve_google_docs_connector_for_mount;
use localityd::hydration::write_parent_database_schema_cache;
use localityd::ipc::{DaemonClientError, DaemonRequest, send_request_with_timeout};
use localityd::runtime::repair_clean_remote_deleted_projections;
use localityd::virtual_fs::{
    VirtualFsChildrenReport, mount_point_identifier, virtual_fs_ancestor_container_identifiers,
    virtual_fs_content_root, virtual_projection_root,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::connect::{
    BrokerOAuthConnectOptions, ConnectError, ConnectOptions, ConnectReport, ConnectionShowReport,
    ConnectionsReport, DisconnectReport, GoogleDocsBrokerOAuthConnectOptions,
    HttpNotionConnectionProbe, OAuthConnectOptions, ProfilesReport,
    run_connect_google_docs_broker_oauth, run_connect_notion, run_connect_notion_broker_oauth,
    run_connect_notion_oauth, run_connection_show, run_connections, run_disconnect, run_profiles,
};
use crate::connector::{
    ConnectorResolveError, SourceDescriptor, resolve_notion_connector_for_mount,
    resolve_source_for_mount_id, resolve_source_for_path, source_descriptor,
};
use crate::create::{CreateError, CreatePageOptions, CreatePageReport, run_create_page};
use crate::daemon::{DaemonControlError, DaemonControlReport, run_daemon_control};
use crate::diff::{DiffError, run_diff_with_state_root};
use crate::doctor::{DoctorOptions, doctor_exit_code, print_doctor_report, run_doctor};
use crate::file_provider as file_provider_helper;
use crate::history::{
    HistoryError, LogOptions, LogReport, UndoReport, run_log, run_undo,
    run_undo_with_applier_at_state_root, undo_report_exit_code,
};
use crate::info::{InfoError, InfoOptions, InfoReport, run_info};
use crate::inspect::{InspectError, InspectOptions, InspectReport, run_inspect};
use crate::local_oauth::{
    LocalOAuthAuthorization, LocalOAuthError, local_redirect, random_state,
    run_local_oauth_authorization,
};
use crate::mount::{MountError, MountOptions, MountReport, run_mount};
use crate::okf::{OkfExportError, OkfExportOptions, OkfExportReport, run_okf_export};
use crate::pull::{PullError, PullReport, run_pull_with_state_root};
use crate::push::{
    PushOptions, PushReport, push_report_exit_code, run_push_with_daemon_at_state_root,
    run_push_with_state_root, select_push_targets,
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
    name = "loc",
    about = "Locality command line interface",
    long_about = "Locality projects remote workspaces, such as Notion, as local Markdown files that can be inspected, edited, pulled, pushed, and reconciled.",
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
    command: Option<LocalityCommand>,
}

#[derive(Debug, Subcommand)]
enum LocalityCommand {
    #[command(about = "Connect Locality to a remote source")]
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
    #[command(about = "Start, stop, reload, or inspect the Locality daemon")]
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
    #[command(about = "Create local draft content in a mounted Locality folder")]
    Create {
        #[command(subcommand)]
        command: CreateCommand,
    },
    #[command(about = "List, validate, and create local template pack workspaces")]
    Templates {
        #[command(subcommand)]
        command: TemplatesCommand,
    },
    #[command(about = "Export mounted content as Open Knowledge Format bundles")]
    Okf {
        #[command(subcommand)]
        command: OkfCommand,
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
    #[command(about = "Run the Locality MCP stdio server")]
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
    #[command(name = "google-docs", about = "Connect Google Docs")]
    GoogleDocs(ConnectGoogleDocsArgs),
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

#[derive(Debug, Args)]
struct ConnectGoogleDocsArgs {
    #[arg(
        long,
        value_name = "ID",
        help = "Connection id to save. Defaults to google-docs-default."
    )]
    name: Option<String>,
    #[arg(long, help = "Print the OAuth URL instead of opening a browser.")]
    no_browser: bool,
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
    #[arg(long, help = "Run localityd as a detached session process.")]
    session: bool,
    #[arg(long, help = "Run localityd with launchd. Supported on macOS only.")]
    launchd: bool,
    #[arg(
        long,
        value_name = "PATH",
        help = "Path to the localityd binary to launch."
    )]
    localityd_bin: Option<String>,
    #[arg(
        long,
        value_name = "PATH",
        help = "Locality state directory. Defaults to $LOCALITY_STATE_DIR or ~/.loc."
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
    #[command(name = "google-docs", about = "Mount Google Docs content")]
    GoogleDocs(MountGoogleDocsArgs),
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
        help = "Mount id to save. Defaults to notion-main, or a connection/root-derived id when needed."
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
struct MountGoogleDocsArgs {
    #[arg(
        value_name = "path",
        help = "Local directory where the mount should be registered."
    )]
    path: String,
    #[arg(
        long,
        value_name = "name-or-id",
        help = "Google Drive workspace folder name, id, or folder URL."
    )]
    workspace_folder: String,
    #[arg(long, value_name = "id", help = "Connection id to use for this mount.")]
    connection: Option<String>,
    #[arg(
        long,
        value_name = "id",
        help = "Mount id to save. Defaults to google-docs-main, or a connection/root-derived id when needed."
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
        help = "Path inside an Locality mount. Defaults to the current scope when omitted."
    )]
    path: Option<String>,
}

#[derive(Debug, Args)]
struct RequiredPathArg {
    #[arg(value_name = "path", help = "Path inside an Locality mount.")]
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
    #[arg(long, help = "Include stale, disconnected, or inactive mount access.")]
    all: bool,
}

#[derive(Debug, Subcommand)]
enum CreateCommand {
    #[command(about = "Create a page directory with page.md")]
    Page(CreatePageArgs),
}

#[derive(Debug, Args)]
struct CreatePageArgs {
    #[arg(long, value_name = "title", help = "Title for the new page.")]
    title: String,
    #[arg(
        long,
        value_name = "dir",
        help = "Existing parent directory where the page should be created. Defaults to the current directory."
    )]
    parent: Option<String>,
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
enum OkfCommand {
    #[command(about = "Export a local projection as an OKF bundle")]
    Export(OkfExportArgs),
}

#[derive(Debug, Args)]
struct OkfExportArgs {
    #[arg(value_name = "path", help = "Local mounted directory to export.")]
    path: String,
    #[arg(
        long,
        value_name = "dir",
        help = "Empty directory to write the OKF bundle into."
    )]
    out: String,
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
        help = "Mount id or path inside an Locality mount."
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
        LocalityCommand::Connect { .. } => connect(&legacy_args[1..], json),
        LocalityCommand::Connections => connections(&legacy_args[1..], json),
        LocalityCommand::Profiles => profiles(&legacy_args[1..], json),
        LocalityCommand::Connection { .. } => connection(&legacy_args[1..], json),
        LocalityCommand::Disconnect(_) => disconnect(&legacy_args[1..], json),
        LocalityCommand::Daemon { .. } => daemon(&legacy_args[1..], json),
        LocalityCommand::Mount { .. } => mount(&legacy_args[1..], json),
        LocalityCommand::Info(_) => info(&legacy_args[1..], json),
        LocalityCommand::Status(_) => status(&legacy_args[1..], json),
        LocalityCommand::Doctor => doctor(json),
        LocalityCommand::Search(_) => search(&legacy_args[1..], json),
        LocalityCommand::Create { .. } => create(&legacy_args[1..], json),
        LocalityCommand::Templates { .. } => templates(&legacy_args[1..], json),
        LocalityCommand::Okf { .. } => okf(&legacy_args[1..], json),
        LocalityCommand::Inspect(_) => inspect(&legacy_args[1..], json),
        LocalityCommand::Pull(_) => pull(&legacy_args[1..], json),
        LocalityCommand::Push(_) => push(&legacy_args[1..], json),
        LocalityCommand::Diff(_) => diff(&legacy_args[1..], json),
        LocalityCommand::Restore(_) => restore(&legacy_args[1..], json),
        LocalityCommand::Undo(_) => undo(&legacy_args[1..], json),
        LocalityCommand::Log(_) => log(&legacy_args[1..], json),
        LocalityCommand::Config => stub("config", json),
        LocalityCommand::Mcp => mcp(),
        LocalityCommand::FileProvider { .. } => file_provider(&legacy_args[1..], json),
    }
}

fn parse_cli(args: &[String]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(
        std::iter::once("loc".to_string())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>(),
    )
}

fn legacy_args_for_command(command: &LocalityCommand) -> Vec<String> {
    let mut args = Vec::new();
    match command {
        LocalityCommand::Connect { command } => {
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
                ConnectCommand::GoogleDocs(options) => {
                    args.push("google-docs".to_string());
                    push_optional_flag_value(&mut args, "--name", options.name.as_deref());
                    push_flag(&mut args, "--no-browser", options.no_browser);
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
        LocalityCommand::Connections => args.push("connections".to_string()),
        LocalityCommand::Profiles => args.push("profiles".to_string()),
        LocalityCommand::Connection { command } => {
            args.push("connection".to_string());
            match command {
                ConnectionCommand::Show(options) => {
                    args.push("show".to_string());
                    args.push(options.id.clone());
                }
            }
        }
        LocalityCommand::Disconnect(options) => {
            args.push("disconnect".to_string());
            args.push(options.id.clone());
        }
        LocalityCommand::Daemon { command } => {
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
        LocalityCommand::Mount { command } => {
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
                MountCommand::GoogleDocs(options) => {
                    args.push("google-docs".to_string());
                    args.push(options.path.clone());
                    args.push("--workspace-folder".to_string());
                    args.push(options.workspace_folder.clone());
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
        LocalityCommand::Info(options) => {
            args.push("info".to_string());
            push_optional_positional(&mut args, options.path.as_deref());
        }
        LocalityCommand::Status(options) => {
            args.push("status".to_string());
            push_optional_positional(&mut args, options.path.as_deref());
        }
        LocalityCommand::Doctor => args.push("doctor".to_string()),
        LocalityCommand::Search(options) => {
            args.push("search".to_string());
            for query_part in &options.query {
                args.push(query_part.clone());
            }
            push_optional_flag_value(&mut args, "--connector", options.connector.as_deref());
            push_flag_value(&mut args, "--limit", &options.limit.to_string());
            push_flag(&mut args, "--all", options.all);
        }
        LocalityCommand::Create { command } => {
            args.push("create".to_string());
            match command {
                CreateCommand::Page(options) => {
                    args.push("page".to_string());
                    push_flag_value(&mut args, "--title", &options.title);
                    push_optional_flag_value(&mut args, "--parent", options.parent.as_deref());
                }
            }
        }
        LocalityCommand::Templates { command } => {
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
        LocalityCommand::Okf { command } => {
            args.push("okf".to_string());
            match command {
                OkfCommand::Export(options) => {
                    args.push("export".to_string());
                    args.push(options.path.clone());
                    push_flag_value(&mut args, "--out", &options.out);
                }
            }
        }
        LocalityCommand::Inspect(options) => {
            args.push("inspect".to_string());
            push_optional_positional(&mut args, options.path.as_deref());
        }
        LocalityCommand::Pull(options) => {
            args.push("pull".to_string());
            args.push(options.path.clone());
        }
        LocalityCommand::Push(options) => {
            args.push("push".to_string());
            args.push(options.path.clone());
            push_flag(&mut args, "--yes", options.yes);
            push_flag(&mut args, "--confirm", options.confirm);
        }
        LocalityCommand::Diff(options) => {
            args.push("diff".to_string());
            args.push(options.path.clone());
        }
        LocalityCommand::Undo(options) => {
            args.push("undo".to_string());
            args.push(options.push_id.clone());
        }
        LocalityCommand::Log(options) => {
            args.push("log".to_string());
            push_optional_positional(&mut args, options.path.as_deref());
        }
        LocalityCommand::Restore(options) => {
            args.push("restore".to_string());
            args.push(options.path.clone());
            push_flag(&mut args, "--force", options.force);
        }
        LocalityCommand::Config => args.push("config".to_string()),
        LocalityCommand::Mcp => args.push("mcp".to_string()),
        LocalityCommand::FileProvider { command } => {
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
    let config = match localityd::mcp::McpServerConfig::discover(&default_state_root()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("loc mcp: {error}");
            return EXIT_INTERNAL;
        }
    };
    match localityd::mcp::serve_stdio(config) {
        Ok(()) => EXIT_SUCCESS,
        Err(error) => {
            eprintln!("loc mcp: {error}");
            EXIT_INTERNAL
        }
    }
}

fn push_daemon_args(args: &mut Vec<String>, options: &DaemonArgs) {
    push_flag(args, "--session", options.session);
    push_flag(args, "--launchd", options.launchd);
    push_optional_flag_value(args, "--localityd-bin", options.localityd_bin.as_deref());
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
    let connector = first_positional(args);
    if connector == Some(GOOGLE_DOCS_CONNECTOR_ID) {
        return connect_google_docs(args, json);
    }
    if connector != Some("notion") {
        return command_error(
            json,
            CommandError::new(
                "connect",
                "usage",
                "usage: loc connect <notion|google-docs> [options] [--json]",
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
                    .with_suggested_command("loc connect notion --token-stdin"),
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
                    .with_suggested_command("loc connect notion --token-stdin"),
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

fn connect_google_docs(args: &[String], json: bool) -> i32 {
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
    let broker_config = match google_docs_oauth_broker_config(args) {
        Ok(config) => config,
        Err(error) => return command_error(json, error, EXIT_INTERNAL),
    };
    let broker = HttpGoogleDocsOAuthBrokerClient::new(broker_config.broker_url.clone());
    let start = match broker.start(&OAuthBrokerStart {
        connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
        redirect_uri: broker_config.redirect_uri,
    }) {
        Ok(start) => start,
        Err(error) => {
            return command_error(
                json,
                CommandError::new(
                    "connect",
                    "oauth_broker_start_failed",
                    format!("Google Docs OAuth broker start failed: {error}"),
                )
                .with_suggested_command("loc connect google-docs"),
                EXIT_INTERNAL,
            );
        }
    };
    let authorization = match run_local_oauth_authorization(
        "Google Docs",
        &start.authorization_url,
        &start.redirect_uri,
        &start.state,
        has_flag(args, "--no-browser"),
        json,
    ) {
        Ok(authorization) => authorization,
        Err(error) => {
            return command_error(
                json,
                google_docs_local_oauth_command_error(error),
                EXIT_INTERNAL,
            );
        }
    };
    let options = GoogleDocsBrokerOAuthConnectOptions {
        connection_id: flag_value(args, "--name").map(ConnectionId::new),
        broker_url: broker_config.broker_url,
        client_id: start.client_id,
        session: start.session,
        state: start.state,
        code: authorization.code,
        redirect_uri: start.redirect_uri,
    };
    match run_connect_google_docs_broker_oauth(&mut store, credentials.as_ref(), options, &broker) {
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
            CommandError::new("connections", "usage", "usage: loc connections [--json]"),
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
            CommandError::new("profiles", "usage", "usage: loc profiles [--json]"),
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
                "usage: loc connection show <id> [--json]",
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
                "usage: loc connection show <id> [--json]",
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
            CommandError::new("disconnect", "usage", "usage: loc disconnect <id> [--json]"),
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
                "usage: loc file-provider register|start|run|stop|status|restart|open|unregister <mount-id-or-path> [--json]",
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
        "list" => file_provider_list(json),
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
                "usage: loc file-provider register|start|run|stop|status|restart|open|unregister|list|reset",
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
                "usage: loc file-provider register <mount-id-or-path> [--json]",
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
        VirtualProjectionRegistration::MacosFileProvider => run_file_provider_helper(
            json,
            "register",
            vec![
                "--mount-id".to_string(),
                daemon_file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID.to_string(),
                "--display-name".to_string(),
                daemon_file_provider::MACOS_FILE_PROVIDER_DISPLAY_NAME.to_string(),
            ],
            Some(mount_id),
        ),
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
                    "usage: loc file-provider {} <mount-id-or-path> [--json]",
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
        VirtualProjectionRegistration::LinuxFuse => run_linux_fuse_lifecycle(json, &mount, action),
        VirtualProjectionRegistration::MacosFileProvider => command_error(
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

fn file_provider_list(json: bool) -> i32 {
    #[cfg(target_os = "linux")]
    {
        let state_root = default_state_root();
        let store = match SqliteStateStore::open(state_root.clone()) {
            Ok(store) => store,
            Err(error) => {
                return command_error(
                    json,
                    CommandError::new("file-provider", "store_open_failed", error.to_string()),
                    EXIT_INTERNAL,
                );
            }
        };
        let mounts = match store.load_mounts() {
            Ok(mounts) => mounts,
            Err(error) => {
                return command_error(
                    json,
                    CommandError::new("file-provider", "store_error", error.to_string()),
                    EXIT_INTERNAL,
                );
            }
        };
        let helper_report = match file_provider_helper::list_linux_fuse_roots(&state_root, &mounts)
        {
            Ok(report) => report,
            Err(error) => {
                return command_error(json, linux_fuse_command_error(error), EXIT_INTERNAL);
            }
        };
        return file_provider_helper_success_report(json, "list", None, helper_report);
    }

    #[cfg(not(target_os = "linux"))]
    {
        run_platform_file_provider_helper(
            json,
            "list",
            windows_cloud_files_state_args_for_platform(),
            None,
        )
    }
}

fn file_provider_run(args: &[String], json: bool) -> i32 {
    let Some(target) = nth_positional(args, 1) else {
        return command_error(
            json,
            CommandError::new(
                "file-provider",
                "usage",
                "usage: loc file-provider run <mount-id-or-path> [--json]",
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
                "usage: loc file-provider open <mount-id-or-path> [--json]",
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
            vec![
                "--mount-id".to_string(),
                daemon_file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID.to_string(),
            ],
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
                "usage: loc file-provider unregister <mount-id-or-path> [--json]",
            ),
            EXIT_USAGE,
        );
    };

    let target_os = std::env::consts::OS;
    if target_os == "linux" {
        if let Ok(store) = SqliteStateStore::open(default_state_root()) {
            if let Ok(mount) = resolve_mount_target(&store, target) {
                if let Err(error) = validate_virtual_projection_registration(&mount, "linux") {
                    return command_error(json, error, EXIT_USAGE);
                }
                let mounts = match store.load_mounts() {
                    Ok(mounts) => mounts,
                    Err(error) => {
                        return command_error(
                            json,
                            CommandError::new(
                                "file-provider",
                                "store_load_failed",
                                error.to_string(),
                            ),
                            EXIT_INTERNAL,
                        );
                    }
                };
                if let Err(error) = guard_linux_fuse_shared_root_unregister(&mounts, &mount) {
                    return command_error(json, error, EXIT_USAGE);
                }
                return run_linux_fuse_unregister(json, Some(&mount), target);
            }
            if let Ok(mounts) = store.load_mounts()
                && let Err(error) = guard_unresolved_linux_fuse_unregister(&mounts, target)
            {
                return command_error(json, error, EXIT_USAGE);
            }
        }
        return run_linux_fuse_unregister(json, None, target);
    }

    let resolved_mount = SqliteStateStore::open(default_state_root())
        .ok()
        .and_then(|store| resolve_mount_target(&store, target).ok());
    if target_os == "windows" {
        if let Ok(store) = SqliteStateStore::open(default_state_root()) {
            match resolve_mount_target(&store, target) {
                Ok(mount) => {
                    if let Err(error) = validate_virtual_projection_registration(&mount, "windows")
                    {
                        return command_error(json, error, EXIT_USAGE);
                    }
                    let mounts = match store.load_mounts() {
                        Ok(mounts) => mounts,
                        Err(error) => {
                            return command_error(
                                json,
                                CommandError::new(
                                    "file-provider",
                                    "store_load_failed",
                                    error.to_string(),
                                ),
                                EXIT_INTERNAL,
                            );
                        }
                    };
                    if let Err(error) =
                        guard_windows_cloud_files_shared_root_unregister(&mounts, &mount)
                    {
                        return command_error(json, error, EXIT_USAGE);
                    }
                }
                Err(_) => {
                    let mounts = match store.load_mounts() {
                        Ok(mounts) => mounts,
                        Err(error) => {
                            return command_error(
                                json,
                                CommandError::new(
                                    "file-provider",
                                    "store_load_failed",
                                    error.to_string(),
                                ),
                                EXIT_INTERNAL,
                            );
                        }
                    };
                    if let Err(error) =
                        guard_unresolved_windows_cloud_files_unregister(&mounts, target)
                    {
                        return command_error(json, error, EXIT_USAGE);
                    }
                }
            }
        }
        let mount_id = resolved_mount
            .map(|mount| mount.mount_id.0)
            .unwrap_or_else(|| target.to_string());
        return run_windows_cloud_files_unregister(json, &mount_id);
    }

    let mount_id = match resolved_mount {
        Some(mount) if mount.projection == ProjectionMode::MacosFileProvider => {
            daemon_file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID.to_string()
        }
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
                "usage: loc restore <path> [--force] [--json]",
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
    let Some(connector) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new("mount", "usage", mount_usage()),
            EXIT_USAGE,
        );
    };
    let descriptor = source_descriptor(connector);

    let Some(root) = nth_positional(args, 1) else {
        return command_error(
            json,
            CommandError::new("mount", "usage", mount_usage()),
            EXIT_USAGE,
        );
    };
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
    let connection_id = match resolve_mount_connection(&store, args, &descriptor) {
        Ok(connection_id) => connection_id,
        Err(error) => return command_error(json, error, EXIT_INTERNAL),
    };
    let explicit_mount_id = flag_value(args, "--mount-id").map(str::to_string);
    let mut mount_id = MountId::new(
        explicit_mount_id
            .clone()
            .unwrap_or_else(|| descriptor.default_mount_id().to_string()),
    );
    let read_only = has_flag(args, "--read-only");
    if let Some(error) = mounted_projection_preflight_error(
        projection.clone(),
        std::env::consts::OS,
        std::env::var_os("LOCALITY_DAEMON_DISABLE").is_some(),
        || virtual_projection_daemon_is_running(&state_root),
    ) {
        return command_error(json, error, EXIT_INTERNAL);
    }
    let remote_root_id = match mount_remote_root_id(
        args,
        &descriptor,
        &store,
        &state_root,
        &mount_id,
        root,
        &connection_id,
        read_only,
        &projection,
    ) {
        Ok(remote_root_id) => remote_root_id,
        Err(error) => {
            let exit_code = mount_remote_root_error_exit_code(&error);
            return command_error(json, error, exit_code);
        }
    };
    if explicit_mount_id.is_none() {
        mount_id = match default_mount_id_for_source(
            &store,
            &descriptor,
            connection_id.as_ref(),
            remote_root_id.as_ref(),
        ) {
            Ok(mount_id) => mount_id,
            Err(error) => return command_error(json, error, EXIT_INTERNAL),
        };
    }

    let options = MountOptions {
        mount_id,
        connector: descriptor.id().to_string(),
        root: PathBuf::from(root),
        remote_root_id,
        connection_id,
        read_only,
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

fn mount_remote_root_id(
    args: &[String],
    descriptor: &SourceDescriptor,
    store: &SqliteStateStore,
    state_root: &Path,
    mount_id: &MountId,
    root: &str,
    connection_id: &Option<ConnectionId>,
    read_only: bool,
    projection: &ProjectionMode,
) -> Result<Option<RemoteId>, CommandError> {
    match descriptor.id() {
        "notion" => {
            let root_page_id = flag_value(args, "--root-page");
            let workspace_mount = has_flag(args, "--workspace");
            if root_page_id.is_some() && workspace_mount {
                return Err(CommandError::new(
                    "mount",
                    "usage",
                    "loc mount notion accepts either --workspace or --root-page <page-id>, not both",
                ));
            }
            if root_page_id.is_none() && !workspace_mount {
                return Err(CommandError::new(
                    "mount",
                    "usage",
                    "loc mount notion requires --workspace or --root-page <page-id>",
                ));
            }
            let remote_root_id = root_page_id.map(RemoteId::new);
            let temp_mount = MountConfig {
                mount_id: mount_id.clone(),
                connector: descriptor.id().to_string(),
                root: PathBuf::from(root),
                remote_root_id: remote_root_id.clone(),
                connection_id: connection_id.clone(),
                read_only,
                projection: projection.clone(),
            };
            let credentials = open_credential_store(state_root);
            resolve_notion_connector_for_mount(store, credentials.as_ref(), &temp_mount)
                .map_err(|error| connector_resolve_command_error("mount", error))?;
            Ok(remote_root_id)
        }
        GOOGLE_DOCS_CONNECTOR_ID => {
            if has_flag(args, "--workspace") || flag_value(args, "--root-page").is_some() {
                return Err(CommandError::new(
                    "mount",
                    "usage",
                    "loc mount google-docs uses --workspace-folder <name-or-id>, not Notion root flags",
                ));
            }
            let Some(workspace_folder) = flag_value(args, "--workspace-folder") else {
                return Err(CommandError::new(
                    "mount",
                    "usage",
                    "loc mount google-docs requires --workspace-folder <name-or-id>",
                ));
            };
            let temp_mount = MountConfig {
                mount_id: mount_id.clone(),
                connector: descriptor.id().to_string(),
                root: PathBuf::from(root),
                remote_root_id: None,
                connection_id: connection_id.clone(),
                read_only,
                projection: projection.clone(),
            };
            let credentials = open_credential_store(state_root);
            let connector =
                resolve_google_docs_connector_for_mount(store, credentials.as_ref(), &temp_mount)
                    .map_err(|error| connector_resolve_command_error("mount", error))?;
            let folder_id = connector
                .resolve_workspace_folder(workspace_folder)
                .map_err(|error| {
                    CommandError::new(
                        "mount",
                        "workspace_folder_error",
                        format!(
                            "failed to resolve Google Docs workspace folder `{workspace_folder}`: {error}"
                        ),
                    )
                    .with_suggested_command("loc connect google-docs")
                })?;
            Ok(Some(folder_id))
        }
        connector => Err(CommandError::new(
            "mount",
            "usage",
            format!("loc mount {connector} is not supported by this build"),
        )),
    }
}

fn mount_remote_root_error_exit_code(error: &CommandError) -> i32 {
    match error.code.as_str() {
        "usage" => EXIT_USAGE,
        _ => EXIT_INTERNAL,
    }
}

fn default_mount_id_for_source<S>(
    store: &S,
    descriptor: &SourceDescriptor,
    connection_id: Option<&ConnectionId>,
    remote_root_id: Option<&RemoteId>,
) -> Result<MountId, CommandError>
where
    S: MountRepository,
{
    let mounts = store
        .load_mounts()
        .map_err(|error| CommandError::new("mount", "store_error", error.to_string()))?;
    let default_mount_id = MountId::new(descriptor.default_mount_id().to_string());
    if mount_id_available_for_source(
        &mounts,
        &default_mount_id,
        descriptor,
        connection_id,
        remote_root_id,
    ) {
        return Ok(default_mount_id);
    }

    let base = source_mount_id_base(descriptor, connection_id, remote_root_id);
    for suffix in 1.. {
        let candidate = if suffix == 1 {
            MountId::new(base.clone())
        } else {
            MountId::new(format!("{base}-{suffix}"))
        };
        if mount_id_available_for_source(
            &mounts,
            &candidate,
            descriptor,
            connection_id,
            remote_root_id,
        ) {
            return Ok(candidate);
        }
    }

    unreachable!("unbounded mount id suffix search should always find a candidate")
}

fn mount_id_available_for_source(
    mounts: &[MountConfig],
    mount_id: &MountId,
    descriptor: &SourceDescriptor,
    connection_id: Option<&ConnectionId>,
    remote_root_id: Option<&RemoteId>,
) -> bool {
    mounts
        .iter()
        .find(|mount| mount.mount_id == *mount_id)
        .is_none_or(|mount| {
            mount.connector == descriptor.id()
                && mount.connection_id.as_ref() == connection_id
                && mount.remote_root_id.as_ref() == remote_root_id
        })
}

fn source_mount_id_base(
    descriptor: &SourceDescriptor,
    connection_id: Option<&ConnectionId>,
    remote_root_id: Option<&RemoteId>,
) -> String {
    if let Some(connection_id) = connection_id {
        return mount_id_with_source_prefix(
            descriptor.id(),
            &mount_id_component(connection_id.as_str()),
        );
    }
    if let Some(remote_root_id) = remote_root_id {
        let component = mount_id_component(remote_root_id.as_str());
        let short = component.chars().take(12).collect::<String>();
        if !short.is_empty() {
            return mount_id_with_source_prefix(descriptor.id(), &short);
        }
    }
    descriptor.default_mount_id().to_string()
}

fn mount_id_with_source_prefix(connector: &str, component: &str) -> String {
    let component = if component.is_empty() {
        "mount"
    } else {
        component
    };
    let prefixed = format!("{connector}-");
    if component == connector || component.starts_with(&prefixed) {
        component.to_string()
    } else {
        format!("{connector}-{component}")
    }
}

fn mount_id_component(value: &str) -> String {
    let mut normalized = String::new();
    let mut last_was_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            normalized.push('-');
            last_was_dash = true;
        }
    }
    let normalized = normalized.trim_matches('-').to_string();
    if normalized.is_empty() {
        "mount".to_string()
    } else {
        normalized
    }
}

fn pull(args: &[String], json: bool) -> i32 {
    let Some(path) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new("pull", "usage", "usage: loc pull <path> [--json]"),
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
            signal_pull_virtual_projection_refresh(&state_root, &report);
            let exit_code = pull_report_exit_code(&report);
            print_json(&report);
            return exit_code;
        }
        DaemonReport::Report(report) => {
            signal_pull_virtual_projection_refresh(&state_root, &report);
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
            signal_pull_virtual_projection_refresh_with_store(&store, &report);
            let exit_code = pull_report_exit_code(&report);
            print_json(&report);
            exit_code
        }
        Ok(report) => {
            signal_pull_virtual_projection_refresh_with_store(&store, &report);
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
        ..StatusOptions::default()
    };
    if let Some(target) = options.path.as_deref() {
        reconcile_projection_changes_best_effort("status", &mut store, &state_root, Some(target));
    }
    let _ = repair_clean_remote_deleted_projections(&mut store, Some(&state_root), None);

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
        include_stale_access: has_flag(args, "--all"),
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

fn create(args: &[String], json: bool) -> i32 {
    match first_positional(args) {
        Some("page") => create_page(args, json),
        _ => command_error(
            json,
            CommandError::new(
                "create",
                "usage",
                "usage: loc create <page> [options] [--json]",
            ),
            EXIT_USAGE,
        ),
    }
}

fn create_page(args: &[String], json: bool) -> i32 {
    let Some(title) = flag_value(args, "--title").map(str::to_string) else {
        return command_error(
            json,
            CommandError::new("create_page", "missing_title", "--title is required"),
            EXIT_USAGE,
        );
    };
    let state_root = default_state_root();
    let store = match SqliteStateStore::open(state_root) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("create_page", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let options = CreatePageOptions {
        title,
        parent: flag_value(args, "--parent").map(PathBuf::from),
    };
    match run_create_page(&store, options) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_create_page_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => create_command_error(json, error),
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
                "localityd did not respond within {}ms while enumerating ancestor metadata for search result `{container_identifier}`",
                daemon_request_timeout().as_millis()
            ),
        )
        .with_suggested_command("loc daemon restart")),
        DaemonReport::Unavailable(DaemonUnavailableReason::Disabled)
        | DaemonReport::Unavailable(DaemonUnavailableReason::NotAvailable) => Err(
            CommandError::new(
                "search",
                "daemon_required",
                format!(
                    "mount `{}` uses projection `{}`; Notion URL search must enumerate ancestor metadata through localityd",
                    mount.mount_id.0,
                    mount.projection.as_str()
                ),
            )
            .with_suggested_command("loc daemon restart"),
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
                    "localityd did not respond within {}ms while refreshing Notion metadata for search",
                    daemon_mutating_request_timeout().as_millis()
                ),
            )
            .with_suggested_command("loc daemon restart"));
        }
        DaemonUnavailableReason::NotAvailable if mount.projection.uses_virtual_filesystem() => {
            return Err(CommandError::new(
                "search",
                "daemon_required",
                format!(
                    "mount `{}` uses projection `{}`; Notion URL search metadata refresh must run through localityd",
                    mount.mount_id.0,
                    mount.projection.as_str()
                ),
            )
            .with_suggested_command("loc daemon restart"));
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
                        "usage: loc templates validate <path> [--json]",
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
                        "usage: loc templates new <pack> <path> [--force] [--json]",
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
                        "usage: loc templates new <pack> <path> [--force] [--json]",
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
                        "usage: loc templates apply <pack> <template> --to <dir> [--title <title>] [--force] [--json]",
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
                        "usage: loc templates apply <pack> <template> --to <dir> [--title <title>] [--force] [--json]",
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
                        "usage: loc templates apply <pack> <template> --to <dir> [--title <title>] [--force] [--json]",
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
                "usage: loc templates list|validate|new|apply [--json]",
            ),
            EXIT_USAGE,
        ),
    }
}

fn okf(args: &[String], json: bool) -> i32 {
    match first_positional(args) {
        Some("export") => okf_export(args, json),
        _ => command_error(
            json,
            CommandError::new(
                "okf",
                "usage",
                "usage: loc okf export <path> --out <dir> [--json]",
            ),
            EXIT_USAGE,
        ),
    }
}

fn okf_export(args: &[String], json: bool) -> i32 {
    let Some(source) = nth_positional(args, 1).map(PathBuf::from) else {
        return command_error(
            json,
            CommandError::new(
                "okf_export",
                "missing_path",
                "source path is required: loc okf export <path> --out <dir>",
            ),
            EXIT_USAGE,
        );
    };
    let Some(output) = flag_value(args, "--out").map(PathBuf::from) else {
        return command_error(
            json,
            CommandError::new("okf_export", "missing_output", "--out <dir> is required"),
            EXIT_USAGE,
        );
    };
    let connector = okf_connector_hint(&source);
    match run_okf_export(OkfExportOptions {
        source,
        output,
        connector,
    }) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_okf_export_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => okf_export_command_error(json, error),
    }
}

fn okf_connector_hint(path: &Path) -> Option<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let store = SqliteStateStore::open(default_state_root()).ok()?;
    let mounts = store.load_mounts().ok()?;
    daemon_file_provider::find_mount_for_path(&mounts, &absolute)
        .map(|(mount, _)| mount.connector.clone())
}

fn inspect(args: &[String], json: bool) -> i32 {
    let Some(path) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new("inspect", "usage", "usage: loc inspect <path> [--json]"),
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
            CommandError::new("undo", "usage", "usage: loc undo <push-id> [--json]"),
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

    if !matches!(
        journal.status,
        JournalStatus::Applied | JournalStatus::Reconciled
    ) {
        return match run_undo(&mut store, push_id) {
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
        };
    }

    let credentials = open_credential_store(&state_root);
    let connector =
        match resolve_source_for_mount_id(&store, credentials.as_ref(), &journal.mount_id) {
            Ok(connector) => connector,
            Err(error) => return connector_command_error("undo", json, error),
        };
    let mut undo_applier = ConnectorUndoApplier::new(&connector);

    match run_undo_with_applier_at_state_root(
        &mut store,
        push_id,
        &mut undo_applier,
        Some(&state_root),
    ) {
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
                "usage: loc push <path> [-y|--yes] [--confirm] [--json]",
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
    if let Err(error) =
        repair_missing_database_schema_for_target("push", &mut store, &state_root, &target_path)
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
        if let Err(error) =
            repair_missing_database_schema_for_target("push", &mut store, &state_root, target)
        {
            return command_error(json, error, EXIT_INTERNAL);
        }
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

    fn from_loc(error: LocalityError) -> Self {
        Self::new(
            locality_error_code(&error),
            error.to_string(),
            locality_error_exit_code(&error),
        )
    }

    fn from_diff(error: DiffError) -> Self {
        Self::new(error.code(), error.message(), diff_error_exit_code(&error))
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
    let preview = run_push_with_state_root(store, &target_path, options.clone(), Some(state_root))
        .map_err(PushCommandError::from_diff)?;
    if preview.pipeline_action != "proceed_to_apply" {
        return Ok(preview);
    }

    verify_daemon_push_plan_matches_cli_preview(state_root, &target_path, &preview)?;

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
                    "localityd did not respond within {}ms after the push request was submitted; refusing direct fallback to avoid duplicate remote writes",
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
        .map_err(PushCommandError::from_loc)
}

fn verify_daemon_push_plan_matches_cli_preview(
    state_root: &Path,
    target_path: &Path,
    cli_preview: &PushReport,
) -> Result<(), PushCommandError> {
    match run_daemon_report::<PushJobReport>(
        state_root,
        &DaemonRequest::Push {
            path: target_path.to_path_buf(),
            assume_yes: false,
            confirm_dangerous: false,
        },
    ) {
        DaemonReport::Report(report) => {
            let daemon_preview = PushReport::from_daemon(report);
            if push_preview_plan_matches(cli_preview, &daemon_preview) {
                Ok(())
            } else {
                Err(PushCommandError::new(
                    "daemon_plan_mismatch",
                    "daemon push plan differs from the CLI diff plan; restart localityd so push uses the same planner as diff",
                    EXIT_INTERNAL,
                ))
            }
        }
        DaemonReport::Unavailable(DaemonUnavailableReason::TimedOut) => Err(PushCommandError::new(
            "daemon_timeout",
            format!(
                "localityd did not respond within {}ms while verifying the push plan; refusing direct fallback to avoid racing daemon writes",
                daemon_mutating_request_timeout().as_millis()
            ),
            EXIT_INTERNAL,
        )),
        DaemonReport::Unavailable(reason) => {
            warn_daemon_fallback("push", reason);
            Ok(())
        }
        DaemonReport::Error(error) => Err(PushCommandError {
            payload: CommandError::new("push", error.code, error.message),
            exit_code: error.exit_code,
        }),
    }
}

fn push_preview_plan_matches(cli_preview: &PushReport, daemon_preview: &PushReport) -> bool {
    cli_preview.validation == daemon_preview.validation
        && cli_preview.plan == daemon_preview.plan
        && cli_preview.guardrail == daemon_preview.guardrail
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
        | StatusError::MountIdNotFound(_)
        | StatusError::Store(locality_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
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
            CommandError::new("diff", "usage", "usage: loc diff <path> [--json]"),
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
    if let Err(error) =
        repair_missing_database_schema_for_target("diff", &mut store, &state_root, &target_path)
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
            "  summary: {} updated, {} replaced, {} media updated, {} created, {} moved, {} archived",
            entry.plan_summary.blocks_updated,
            entry.plan_summary.blocks_replaced,
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
            println!("  then: loc push {path} -y");
        } else {
            println!("  then: run `loc push <file> -y` for each resolved file");
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
        if mount.live_mode.enabled || mount.live_mode.state != "off" {
            println!("{}  live_mode: {}", mount.mount_id, mount.live_mode.state);
            if let Some(reason) = mount.live_mode.reason.as_deref() {
                println!("  live_mode_reason: {reason}");
            }
        }
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

fn print_create_page_report(report: &CreatePageReport) {
    println!("created {}", report.path);
    println!("  title: {}", report.title);
    println!("  mount: {}", report.mount_id);
    println!("  next:");
    for next in &report.next {
        println!("    {next}");
    }
}

fn print_okf_export_report(report: &OkfExportReport) {
    println!("exported OKF bundle {}", report.output);
    println!("  source: {}", report.source);
    println!("  concepts: {}", report.concepts);
    println!("  indexes: {}", report.indexes);
    if !report.skipped.is_empty() {
        println!("  skipped:");
        for skipped in &report.skipped {
            println!("    {} ({})", skipped.path, skipped.reason);
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

fn inspect_side_summary(side: &locality_core::explain::RemoteChangeSide) -> String {
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

#[cfg(target_os = "linux")]
fn run_linux_fuse_lifecycle(
    json: bool,
    mount: &MountConfig,
    action: file_provider_helper::WindowsCloudFilesLifecycleAction,
) -> i32 {
    let state_root = default_state_root();
    let action = match action {
        file_provider_helper::WindowsCloudFilesLifecycleAction::Start => {
            file_provider_helper::LinuxFuseLifecycleAction::Start
        }
        file_provider_helper::WindowsCloudFilesLifecycleAction::Stop => {
            file_provider_helper::LinuxFuseLifecycleAction::Stop
        }
        file_provider_helper::WindowsCloudFilesLifecycleAction::Status => {
            file_provider_helper::LinuxFuseLifecycleAction::Status
        }
        file_provider_helper::WindowsCloudFilesLifecycleAction::Restart => {
            file_provider_helper::LinuxFuseLifecycleAction::Restart
        }
    };
    let helper_report =
        match file_provider_helper::run_linux_fuse_lifecycle(&state_root, mount, action) {
            Ok(report) => report,
            Err(error) => {
                return command_error(json, linux_fuse_command_error(error), EXIT_INTERNAL);
            }
        };
    file_provider_helper_success_report(
        json,
        action.as_str(),
        Some(mount.mount_id.0.clone()),
        helper_report,
    )
}

#[cfg(not(target_os = "linux"))]
fn run_linux_fuse_lifecycle(
    json: bool,
    mount: &MountConfig,
    action: file_provider_helper::WindowsCloudFilesLifecycleAction,
) -> i32 {
    command_error(
        json,
        CommandError::new(
            "file-provider",
            "unsupported_platform",
            format!(
                "file-provider {} is only supported for Linux FUSE mounts on Linux; mount `{}` cannot {} here",
                action.as_str(),
                mount.mount_id.0,
                action.as_str()
            ),
        ),
        EXIT_USAGE,
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
            "locality_fuse": registration.locality_fuse.display().to_string(),
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

fn guard_linux_fuse_shared_root_unregister(
    mounts: &[MountConfig],
    target: &MountConfig,
) -> Result<(), CommandError> {
    guard_shared_virtual_projection_root_unregister(
        mounts,
        target,
        ProjectionMode::LinuxFuse,
        "linux_fuse_shared_root_in_use",
        "Linux FUSE",
    )
}

fn guard_windows_cloud_files_shared_root_unregister(
    mounts: &[MountConfig],
    target: &MountConfig,
) -> Result<(), CommandError> {
    guard_shared_virtual_projection_root_unregister(
        mounts,
        target,
        ProjectionMode::WindowsCloudFiles,
        "windows_cloud_files_shared_root_in_use",
        "Windows Cloud Files",
    )
}

fn guard_unresolved_windows_cloud_files_unregister(
    mounts: &[MountConfig],
    target: &str,
) -> Result<(), CommandError> {
    guard_unresolved_virtual_projection_unregister(
        mounts,
        target,
        ProjectionMode::WindowsCloudFiles,
        "windows_cloud_files_unresolved_shared_root",
        "Windows Cloud Files",
    )
}

fn guard_unresolved_linux_fuse_unregister(
    mounts: &[MountConfig],
    target: &str,
) -> Result<(), CommandError> {
    guard_unresolved_virtual_projection_unregister(
        mounts,
        target,
        ProjectionMode::LinuxFuse,
        "linux_fuse_unresolved_shared_root",
        "Linux FUSE",
    )
}

fn guard_unresolved_virtual_projection_unregister(
    mounts: &[MountConfig],
    target: &str,
    projection: ProjectionMode,
    code: &'static str,
    label: &'static str,
) -> Result<(), CommandError> {
    let mut mount_ids = mounts
        .iter()
        .filter(|mount| mount.projection == projection)
        .map(|mount| mount.mount_id.0.clone())
        .collect::<Vec<_>>();
    mount_ids.sort();
    if mount_ids.is_empty() {
        return Ok(());
    }

    Err(CommandError::new(
        "file-provider",
        code,
        format!(
            "{label} unregister target `{target}` does not match a known mount; refusing raw unregister while shared {label} mounts exist: {}",
            mount_ids.join(", ")
        ),
    ))
}

fn guard_shared_virtual_projection_root_unregister(
    mounts: &[MountConfig],
    target: &MountConfig,
    projection: ProjectionMode,
    code: &'static str,
    label: &'static str,
) -> Result<(), CommandError> {
    if target.projection != projection {
        return Ok(());
    }
    let target_root = virtual_projection_root(target);
    let mut sibling_ids = mounts
        .iter()
        .filter(|mount| {
            mount.mount_id != target.mount_id
                && mount.projection == projection
                && virtual_projection_root(mount) == target_root
        })
        .map(|mount| mount.mount_id.0.clone())
        .collect::<Vec<_>>();
    sibling_ids.sort();
    if sibling_ids.is_empty() {
        return Ok(());
    }

    Err(CommandError::new(
        "file-provider",
        code,
        format!(
            "{label} root `{}` is shared by sibling mount ids {}; unregistering `{}` would stop their provider too",
            target_root.display(),
            sibling_ids.join(", "),
            target.mount_id.0
        ),
    ))
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
    let unit_id = mount
        .map(file_provider_helper::linux_fuse_root_id)
        .unwrap_or_else(|| mount_id.clone());
    let unit_name = file_provider_helper::linux_fuse_unit_name(&unit_id);
    let unit_path = match file_provider_helper::linux_fuse_unit_path(&unit_name) {
        Ok(path) => path,
        Err(error) => return command_error(json, linux_fuse_command_error(error), EXIT_INTERNAL),
    };

    let _ = file_provider_helper::run_systemctl_user(&["disable", "--now", &unit_name]);
    if let Some(mount) = mount {
        let projection_root = localityd::virtual_fs::virtual_projection_root(mount);
        let _ = ProcessCommand::new("fusermount3")
            .arg("-uz")
            .arg(&projection_root)
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
            command_error.with_suggested_command("loc daemon start")
        }
        _ => command_error,
    }
}

fn print_file_provider_report(report: &FileProviderCommandReport) {
    if report.action == "list" {
        for line in file_provider_list_lines(report) {
            println!("{line}");
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

fn file_provider_list_lines(report: &FileProviderCommandReport) -> Vec<String> {
    if let Some(roots) = report.helper_report.get("roots").and_then(Value::as_array) {
        let mut lines = Vec::new();
        let linux_roots = roots
            .iter()
            .any(|root| root.get("mount_ids").is_some() || root.get("mountpoint").is_some());
        if linux_roots {
            for root in roots {
                let mount_ids = root
                    .get("mount_ids")
                    .and_then(Value::as_array)
                    .map(|mount_ids| {
                        mount_ids
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .filter(|mount_ids| !mount_ids.is_empty())
                    .unwrap_or_else(|| "<unknown>".to_string());
                let mountpoint = root
                    .get("mountpoint")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown>");
                let registered = root
                    .get("registered")
                    .and_then(Value::as_bool)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let active = root
                    .get("active")
                    .and_then(Value::as_bool)
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                lines.push(format!(
                    "linux-fuse\t{mount_ids}\t{mountpoint}\tregistered={registered}\tactive={active}"
                ));
            }
            if let Some(stale_units) = report
                .helper_report
                .get("stale_units")
                .and_then(Value::as_array)
            {
                for unit in stale_units {
                    let service = unit
                        .get("service")
                        .and_then(Value::as_str)
                        .unwrap_or("<unknown>");
                    let mountpoint = unit
                        .get("mountpoint")
                        .and_then(Value::as_str)
                        .unwrap_or("<unknown>");
                    let unit_path = unit
                        .get("unit_path")
                        .and_then(Value::as_str)
                        .unwrap_or("<unknown>");
                    let legacy = unit
                        .get("legacy")
                        .and_then(Value::as_bool)
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    lines.push(format!(
                        "stale-linux-fuse\t{service}\t{mountpoint}\t{unit_path}\tlegacy={legacy}"
                    ));
                }
            }
            return if lines.is_empty() {
                vec!["no file provider domains".to_string()]
            } else {
                lines
            };
        }

        if roots.is_empty() {
            return vec!["no file provider domains".to_string()];
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
            lines.push(format!("{mount_id}\t{display_name}\t{path}"));
        }
        return lines;
    }
    let Some(domains) = report
        .helper_report
        .get("domains")
        .and_then(Value::as_array)
    else {
        return vec!["no file provider domains".to_string()];
    };
    if domains.is_empty() {
        return vec!["no file provider domains".to_string()];
    }
    domains
        .iter()
        .map(|domain| {
            let identifier = domain
                .get("identifier")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            let display_name = domain
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            format!("{identifier}\t{display_name}")
        })
        .collect()
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
    daemon_file_provider::find_mount_for_path(&mounts, &target_path)
        .map(|(mount, _)| mount.clone())
        .ok_or_else(|| format!("no Locality mount matches `{target}`"))
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
    file_provider_helper::windows_cloud_files_display_name(&mount.root, &mount.mount_id.0)
}

fn stub(command: &str, json: bool) -> i32 {
    if json {
        println!("{{\"ok\":false,\"command\":\"{command}\",\"error\":\"not_implemented\"}}");
    } else {
        println!("loc {command}: not implemented yet");
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
        "{} block{} updated, {} replaced, {} media updated, {} block{} created, {} entit{} created, {} moved, {} block{} archived, {} entit{} archived",
        plan.summary.blocks_updated,
        plural(plan.summary.blocks_updated),
        plan.summary.blocks_replaced,
        plan.summary.media_updated,
        plan.summary.blocks_created,
        plural(plan.summary.blocks_created),
        plan.summary.entities_created,
        if plan.summary.entities_created == 1 {
            "y"
        } else {
            "ies"
        },
        plan.summary.blocks_moved,
        plan.summary.blocks_archived,
        plural(plan.summary.blocks_archived),
        plan.summary.entities_archived,
        if plan.summary.entities_archived == 1 {
            "y"
        } else {
            "ies"
        }
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct GoogleDocsOAuthBrokerCliConfig {
    broker_url: String,
    redirect_uri: String,
}

fn notion_oauth_config(args: &[String]) -> Result<NotionOAuthCliConfig, CommandError> {
    let client_id = env_first(&["LOCALITY_NOTION_OAUTH_CLIENT_ID", "NOTION_OAUTH_CLIENT_ID"])
        .ok_or_else(|| missing_oauth_config("LOCALITY_NOTION_OAUTH_CLIENT_ID"))?;
    let client_secret = env_first(&[
        "LOCALITY_NOTION_OAUTH_CLIENT_SECRET",
        "NOTION_OAUTH_CLIENT_SECRET",
    ])
    .ok_or_else(|| missing_oauth_config("LOCALITY_NOTION_OAUTH_CLIENT_SECRET"))?;
    let redirect_uri = flag_value(args, "--redirect-uri")
        .map(str::to_string)
        .or_else(|| {
            env_first(&[
                "LOCALITY_NOTION_OAUTH_REDIRECT_URI",
                "NOTION_OAUTH_REDIRECT_URI",
            ])
        })
        .unwrap_or_else(|| "http://localhost:8757/oauth/notion/callback".to_string());

    local_redirect(&redirect_uri).map_err(|error| {
        CommandError::new("connect", error.code, error.message)
            .with_suggested_command("loc connect notion --token-stdin")
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
        .or_else(|| {
            env_first(&[
                "LOCALITY_NOTION_OAUTH_BROKER_URL",
                "LOCALITY_AUTH_BROKER_URL",
            ])
        })
        .unwrap_or_else(|| DEFAULT_LOCALITY_NOTION_OAUTH_BROKER_URL.to_string());
    let redirect_uri = flag_value(args, "--redirect-uri")
        .map(str::to_string)
        .or_else(|| {
            env_first(&[
                "LOCALITY_NOTION_OAUTH_REDIRECT_URI",
                "NOTION_OAUTH_REDIRECT_URI",
            ])
        })
        .unwrap_or_else(|| "http://localhost:8757/oauth/notion/callback".to_string());

    local_redirect(&redirect_uri).map_err(|error| {
        CommandError::new("connect", error.code, error.message)
            .with_suggested_command("loc connect notion --token-stdin")
    })?;

    Ok(NotionOAuthBrokerCliConfig {
        broker_url,
        redirect_uri,
    })
}

fn google_docs_oauth_broker_config(
    args: &[String],
) -> Result<GoogleDocsOAuthBrokerCliConfig, CommandError> {
    let broker_url = flag_value(args, "--broker-url")
        .map(str::to_string)
        .or_else(|| {
            env_first(&[
                "LOCALITY_GOOGLE_DOCS_OAUTH_BROKER_URL",
                "LOCALITY_AUTH_BROKER_URL",
            ])
        })
        .unwrap_or_else(|| DEFAULT_GOOGLE_DOCS_OAUTH_BROKER_URL.to_string());
    let redirect_uri = flag_value(args, "--redirect-uri")
        .map(str::to_string)
        .or_else(|| env_first(&["LOCALITY_GOOGLE_DOCS_OAUTH_REDIRECT_URI"]))
        .unwrap_or_else(|| DEFAULT_GOOGLE_DOCS_OAUTH_REDIRECT_URI.to_string());

    local_redirect(&redirect_uri).map_err(|error| {
        CommandError::new("connect", error.code, error.message)
            .with_suggested_command("loc connect google-docs")
    })?;

    Ok(GoogleDocsOAuthBrokerCliConfig {
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
    .with_suggested_command("loc connect notion --token-stdin")
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

fn notion_authorize_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "{DEFAULT_NOTION_OAUTH_AUTHORIZE_URL}?client_id={}&response_type=code&owner=user&redirect_uri={}&state={}",
        url_encode(client_id),
        url_encode(redirect_uri),
        url_encode(state)
    )
}

fn url_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| std::env::var(key).ok())
        .filter(|value| !value.is_empty())
}

fn local_oauth_command_error(error: LocalOAuthError) -> CommandError {
    let command_error = CommandError::new("connect", error.code, error.message);
    if command_error.code == "invalid_redirect_uri" {
        command_error.with_suggested_command("loc connect notion --token-stdin")
    } else {
        command_error
    }
}

fn google_docs_local_oauth_command_error(error: LocalOAuthError) -> CommandError {
    let command_error = CommandError::new("connect", error.code, error.message);
    if command_error.code == "invalid_redirect_uri" {
        command_error.with_suggested_command("loc connect google-docs")
    } else {
        command_error
    }
}

fn warn_daemon_fallback(command: &str, reason: DaemonUnavailableReason) {
    if std::env::var("LOCALITY_DAEMON_DISABLE").is_err() {
        match reason {
            DaemonUnavailableReason::TimedOut => eprintln!(
                "localityd did not respond within {}ms; executing {command} directly",
                daemon_mutating_request_timeout().as_millis()
            ),
            DaemonUnavailableReason::NotAvailable => eprintln!(
                "localityd not running; executing {command} directly (start localityd for background hydration)"
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
                    "localityd did not respond within {}ms after the pull request was submitted; refusing direct fallback to avoid racing daemon hydration",
                    daemon_mutating_request_timeout().as_millis()
                ),
            )
            .with_suggested_command("loc daemon restart"),
        ),
        DaemonUnavailableReason::NotAvailable
            if mount.is_some_and(|mount| mount.projection.uses_virtual_filesystem()) =>
        {
            Some(
                CommandError::new(
                    "pull",
                    "daemon_required",
                    format!(
                        "mount `{}` uses projection `{}`; pull for virtual projections must run through localityd so the provider cache stays serialized",
                        mount.expect("checked mount").mount_id.0,
                        mount.expect("checked mount").projection.as_str()
                    ),
                )
                .with_suggested_command("loc daemon restart"),
            )
        }
        DaemonUnavailableReason::Disabled | DaemonUnavailableReason::NotAvailable => None,
    }
}

fn signal_pull_virtual_projection_refresh(state_root: &Path, report: &PullReport) {
    if report.enumerated == 0 && report.stubbed == 0 {
        return;
    }
    let Ok(store) = SqliteStateStore::open(state_root.to_path_buf()) else {
        return;
    };
    signal_pull_virtual_projection_refresh_with_store(&store, report);
}

fn signal_pull_virtual_projection_refresh_with_store(
    store: &SqliteStateStore,
    report: &PullReport,
) {
    if report.enumerated == 0 && report.stubbed == 0 {
        return;
    }
    let Ok(Some((mount, container_identifier))) =
        pull_virtual_projection_signal_target(store, report)
    else {
        return;
    };
    let _ = file_provider_helper::signal_macos_file_provider_container(
        &mount.mount_id.0,
        &container_identifier,
    );
}

fn pull_virtual_projection_signal_target(
    store: &SqliteStateStore,
    report: &PullReport,
) -> Result<Option<(MountConfig, String)>, locality_store::StoreError> {
    let mount_id = MountId::new(report.mount_id.clone());
    let Some(mount) = store.get_mount(&mount_id)? else {
        return Ok(None);
    };
    if mount.projection != ProjectionMode::MacosFileProvider {
        return Ok(None);
    }

    let target = absolute_command_path(Path::new(&report.target));
    let Some(matched) = daemon_file_provider::match_mount_path(&mount, &target) else {
        return Ok(None);
    };
    let relative_path = matched.relative_path;
    if relative_path.as_os_str().is_empty() {
        return Ok(Some((
            mount,
            daemon_file_provider::ROOT_CONTAINER_IDENTIFIER.to_string(),
        )));
    }

    if let Some(entity) = store.find_entity_by_path(&mount.mount_id, &relative_path)? {
        return Ok(match entity.kind {
            EntityKind::Database | EntityKind::Page => Some((mount, entity.remote_id.0)),
            EntityKind::Directory | EntityKind::Asset | EntityKind::Unknown(_) => {
                Some((mount, format!("path:{}", entity.path.display())))
            }
        });
    }

    if let Some(entity) = store
        .list_entities(&mount.mount_id)?
        .into_iter()
        .find(|entity| {
            entity.kind == EntityKind::Page && page_container_path(&entity.path) == relative_path
        })
    {
        return Ok(Some((mount, format!("children:{}", entity.remote_id.0))));
    }

    let container_identifier = mount_point_identifier(&mount);
    Ok(Some((mount, container_identifier)))
}

fn resolve_mount_connection(
    store: &SqliteStateStore,
    args: &[String],
    descriptor: &SourceDescriptor,
) -> Result<Option<ConnectionId>, CommandError> {
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
    if std::env::var("LOCALITY_DAEMON_DISABLE").is_ok() {
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
    std::env::var("LOCALITY_DAEMON_REQUEST_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_DAEMON_CONTROL_TIMEOUT)
}

fn daemon_mutating_request_timeout() -> Duration {
    std::env::var("LOCALITY_DAEMON_REQUEST_TIMEOUT_MS")
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
    if std::env::var("LOCALITY_DAEMON_DISABLE").is_ok() {
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
                    "loc mount: daemon mount reload failed: {}: {}",
                    error.code, error.message
                );
            }
        }
        Err(DaemonClientError::NotAvailable(_) | DaemonClientError::TimedOut(_)) => {}
        Err(error) => eprintln!("loc mount: daemon mount reload failed: {}", error.message()),
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
        eprintln!("loc {}: {}", error.command, error.message);
        if let Some(suggested_command) = &error.suggested_command {
            eprintln!("hint: {suggested_command}");
        }
    }

    exit_code
}

fn connect_command_error(command: &'static str, json: bool, error: ConnectError) -> i32 {
    let exit_code = match &error {
        ConnectError::ConnectionNameRequired(_) => EXIT_USAGE,
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

fn connector_resolve_command_error(
    command: &'static str,
    error: ConnectorResolveError,
) -> CommandError {
    let mut payload = CommandError::new(command, error.code(), error.message());
    if let Some(suggested_command) = error.suggested_command() {
        payload = payload.with_suggested_command(suggested_command);
    }
    payload
}

fn history_command_error(command: &'static str, json: bool, error: HistoryError) -> i32 {
    let exit_code = history_error_exit_code(&error);
    command_error(
        json,
        CommandError::new(command, error.code(), error.message()),
        exit_code,
    )
}

fn create_command_error(json: bool, error: CreateError) -> i32 {
    let exit_code = match &error {
        CreateError::CurrentDir { .. }
        | CreateError::InvalidTitle(_)
        | CreateError::MountNotFound(_)
        | CreateError::ReadOnlyMount { .. }
        | CreateError::TargetExists(_) => EXIT_USAGE,
        CreateError::Store(_) | CreateError::WriteFile { .. } => EXIT_INTERNAL,
    };
    command_error(
        json,
        CommandError::new("create_page", error.code(), error.message()),
        exit_code,
    )
}

fn okf_export_command_error(json: bool, error: OkfExportError) -> i32 {
    let exit_code = match &error {
        OkfExportError::CurrentDir { .. }
        | OkfExportError::OutputInsideSource { .. }
        | OkfExportError::OutputNotDirectory(_)
        | OkfExportError::OutputNotEmpty(_)
        | OkfExportError::OutputPathConflict { .. }
        | OkfExportError::SourceMissing(_)
        | OkfExportError::SourceNotDirectory(_) => EXIT_USAGE,
        OkfExportError::WalkDirectory { .. }
        | OkfExportError::WriteFile { .. }
        | OkfExportError::YamlSerialize(_) => EXIT_INTERNAL,
    };
    command_error(
        json,
        CommandError::new("okf_export", error.code(), error.message()),
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
    let exit_code = match &error {
        MountError::MountPointConflict { .. } => EXIT_USAGE,
        _ => EXIT_INTERNAL,
    };

    command_error(
        json,
        CommandError::new("mount", error.code(), error.message()),
        exit_code,
    )
}

fn pull_command_error(json: bool, error: PullError) -> i32 {
    let exit_code = match &error {
        PullError::MountNotFound(_)
        | PullError::Store(locality_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
        PullError::ReadFile { .. } | PullError::WriteFile { .. } => EXIT_INTERNAL,
        PullError::Store(_)
        | PullError::Connector(_)
        | PullError::CurrentDir(_)
        | PullError::Projection(_) => EXIT_INTERNAL,
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
        | StatusError::MountIdNotFound(_)
        | StatusError::Store(locality_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
        StatusError::CurrentDir(_) | StatusError::Store(_) => EXIT_INTERNAL,
    };
    let message = match &error {
        StatusError::MountNotFound(_) | StatusError::MountIdNotFound(_) => {
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
        | InspectError::Store(locality_store::StoreError::EntityPathMissing { .. })
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
        | RestoreError::Store(locality_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
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
        | InfoError::Store(locality_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
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
        | HistoryError::Store(locality_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
        HistoryError::Store(_) => EXIT_INTERNAL,
    }
}

fn locality_error_exit_code(error: &LocalityError) -> i32 {
    match error {
        LocalityError::Validation(_) => EXIT_VALIDATION,
        LocalityError::RemoteNotFound(_) => 5,
        LocalityError::NotImplemented(_) => 5,
        _ => EXIT_INTERNAL,
    }
}

fn locality_error_code(error: &LocalityError) -> &'static str {
    match error {
        LocalityError::Validation(_) => "validation_failed",
        LocalityError::Conflict(_) => "conflict",
        LocalityError::Guardrail(_) => "guardrail",
        LocalityError::RemoteNotFound(_) => "remote_not_found",
        LocalityError::InvalidState(_) => "invalid_state",
        LocalityError::Unsupported(_) => "unsupported",
        LocalityError::NotImplemented(_) => "not_implemented",
        LocalityError::Io(_) => "io_error",
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
    locality_platform::mount_cli_capabilities_for_target(target_os)
        .projection_from_cli_value(flag_value(args, "--projection"))
        .map_err(|error| error.message())
}

fn mount_usage() -> String {
    format!(
        "usage: loc mount notion <path> (--workspace|--root-page <page-id>) [--connection <id>] [--mount-id <id>] [--projection {0}] [--read-only] [--json]\n       loc mount google-docs <path> --workspace-folder <name-or-id> [--connection <id>] [--mount-id <id>] [--projection {0}] [--read-only] [--json]",
        projection_usage_options_for_target(std::env::consts::OS)
    )
}

fn projection_usage_options_for_target(target_os: &str) -> String {
    locality_platform::mount_cli_capabilities_for_target(target_os).projection_usage_options()
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
        locality_platform::capabilities::projection_cli_value(&self.projection())
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
    match locality_platform::mount_cli_capabilities_for_target(target_os).virtual_registration {
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

fn mounted_projection_preflight_error(
    projection: ProjectionMode,
    target_os: &str,
    daemon_disabled: bool,
    daemon_running: impl FnOnce() -> bool,
) -> Option<CommandError> {
    let registration =
        auto_registration_for_mounted_projection(projection, target_os, daemon_disabled)?;
    match registration {
        VirtualProjectionRegistration::LinuxFuse if !daemon_running() => Some(
            CommandError::new(
                "mount",
                "daemon_not_running",
                "localityd is not running; start it with `loc daemon start` before mounting a Linux FUSE projection",
            )
            .with_suggested_command("loc daemon start"),
        ),
        _ => None,
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn virtual_projection_daemon_is_running(state_root: &Path) -> bool {
    file_provider_helper::daemon_is_running(state_root)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn virtual_projection_daemon_is_running(_state_root: &Path) -> bool {
    false
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
        std::env::var_os("LOCALITY_DAEMON_DISABLE").is_some(),
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
                        "loc file-provider register {}",
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
                    "loc file-provider register {}",
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
            | "--title"
            | "--parent"
    )
}

fn default_state_root() -> PathBuf {
    locality_platform::default_state_root()
}

fn reconcile_projection_changes(
    command: &'static str,
    store: &mut SqliteStateStore,
    state_root: &Path,
    target: Option<&Path>,
) -> Result<(), CommandError> {
    daemon_file_provider::reconcile_visible_projection(store, state_root, target)
        .map(|_| ())
        .map_err(|error| {
            CommandError::new(
                command,
                "projection_reconcile_failed",
                format!("failed to reconcile visible projection changes: {error}"),
            )
        })
}

fn repair_missing_database_schema_for_target(
    command: &'static str,
    store: &mut SqliteStateStore,
    state_root: &Path,
    target_path: &Path,
) -> Result<(), CommandError> {
    let absolute_path = absolute_command_path(target_path);
    let mounts = store
        .load_mounts()
        .map_err(|error| CommandError::new(command, "store_error", error.to_string()))?;
    let Some((mount, matched)) = daemon_file_provider::find_mount_for_path(&mounts, &absolute_path)
    else {
        return Ok(());
    };
    if mount.connector != "notion" {
        return Ok(());
    }

    let mut relative_path = matched.relative_path;
    let mut entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(|error| CommandError::new(command, "store_error", error.to_string()))?;
    if entity.is_none() && absolute_path.is_dir() {
        let page_relative_path = page_document_path(&relative_path);
        if let Some(page_entity) = store
            .find_entity_by_path(&mount.mount_id, &page_relative_path)
            .map_err(|error| CommandError::new(command, "store_error", error.to_string()))?
        {
            relative_path = page_relative_path;
            entity = Some(page_entity);
        }
    }
    let Some(entity) = entity.filter(|entity| entity.kind == EntityKind::Page) else {
        return Ok(());
    };

    let parent_path = page_listing_parent_path(&relative_path);
    let Some(database) = store
        .find_entity_by_path(&mount.mount_id, &parent_path)
        .map_err(|error| CommandError::new(command, "store_error", error.to_string()))?
        .filter(|entity| entity.kind == EntityKind::Database)
    else {
        return Ok(());
    };

    let output_root = if mount.projection.uses_virtual_filesystem() {
        virtual_fs_content_root(state_root, &mount.mount_id)
    } else {
        mount.root.clone()
    };
    if output_root
        .join(&database.path)
        .join("_schema.yaml")
        .exists()
    {
        return Ok(());
    }

    let credentials = open_credential_store(state_root);
    let connector =
        resolve_source_for_path(store, credentials.as_ref(), &absolute_path).map_err(|error| {
            CommandError::new(command, error.code(), error.message())
                .with_optional_suggested_command(error.suggested_command())
        })?;
    write_parent_database_schema_cache(store, &connector, mount, &entity, &output_root)
        .map(|_| ())
        .map_err(|error| {
            CommandError::new(
                command,
                locality_error_code(&error),
                format!("failed to repair Notion database schema cache: {error}"),
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
        daemon_file_provider::reconcile_visible_projection(store, state_root, target)
    {
        eprintln!("loc {command}: skipped visible projection reconciliation: {error}");
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
    #[cfg(target_os = "windows")]
    use std::fs;
    use std::io::Cursor;
    use std::path::Path;
    #[cfg(target_os = "windows")]
    use std::path::PathBuf;
    #[cfg(target_os = "windows")]
    use std::time::{SystemTime, UNIX_EPOCH};

    use clap::Parser;
    use clap::error::ErrorKind;

    use locality_core::model::{MountId, RemoteId};
    use locality_google_docs::GOOGLE_DOCS_CONNECTOR_ID;
    #[cfg(target_os = "windows")]
    use locality_store::SqliteStateStore;
    use locality_store::{
        ConnectionId, InMemoryStateStore, MountConfig, MountRepository, ProjectionMode,
    };

    use crate::diff::{DiffReport, GuardrailOutput};
    use crate::local_oauth::{local_redirect, parse_oauth_callback};
    use crate::push::PushReport;
    use crate::search::{SearchOptions, SearchReport};

    #[cfg(target_os = "windows")]
    use super::resolve_mount_target;
    use super::{
        Cli, DaemonUnavailableReason, EXIT_SUCCESS, EXIT_VALIDATION, FileProviderCommandReport,
        VirtualProjectionRegistration, absolute_command_path,
        auto_registration_for_mounted_projection, default_mount_id_for_source,
        diff_report_exit_code, file_provider_list_lines, google_docs_oauth_broker_config,
        guard_linux_fuse_shared_root_unregister, guard_unresolved_linux_fuse_unregister,
        guard_unresolved_windows_cloud_files_unregister,
        guard_windows_cloud_files_shared_root_unregister, legacy_args_for_command,
        mounted_projection_preflight_error, notion_authorize_url, notion_oauth_broker_config,
        projection_mode_for_target, projection_usage_options_for_target,
        prompt_for_push_confirmation, pull_direct_fallback_error,
        should_prompt_for_push_confirmation, should_refresh_notion_url_search,
        spinner_config_for_command, spinner_enabled, validate_virtual_projection_registration,
    };

    #[test]
    fn clap_help_is_available_for_commands_and_nested_subcommands() {
        let cases = vec![
            (
                vec!["--help"],
                vec!["Usage: loc", "Commands:", "push", "file-provider"],
            ),
            (
                vec!["connect", "--help"],
                vec![
                    "Usage: loc connect",
                    "Commands:",
                    "notion",
                    "google-docs",
                    "--json",
                ],
            ),
            (
                vec!["connect", "notion", "--help"],
                vec![
                    "Usage: loc connect notion",
                    "Connect a Notion workspace",
                    "--token-stdin",
                    "--direct-oauth",
                ],
            ),
            (
                vec!["connect", "google-docs", "--help"],
                vec![
                    "Usage: loc connect google-docs",
                    "Connect Google Docs",
                    "--broker-url",
                    "--redirect-uri",
                ],
            ),
            (
                vec!["connections", "--help"],
                vec!["Usage: loc connections", "List saved source", "--json"],
            ),
            (
                vec!["profiles", "--help"],
                vec!["Usage: loc profiles", "List connector profiles", "--json"],
            ),
            (
                vec!["connection", "--help"],
                vec!["Usage: loc connection", "Commands:", "show", "--json"],
            ),
            (
                vec!["connection", "show", "--help"],
                vec![
                    "Usage: loc connection show",
                    "Show connection details",
                    "id",
                    "--json",
                ],
            ),
            (
                vec!["disconnect", "--help"],
                vec!["Usage: loc disconnect", "Disconnect", "id", "--json"],
            ),
            (
                vec!["daemon", "--help"],
                vec!["Usage: loc daemon", "Commands:", "start", "restart"],
            ),
            (
                vec!["daemon", "start", "--help"],
                vec![
                    "Usage: loc daemon start",
                    "Start the daemon",
                    "--session",
                    "--localityd-bin",
                ],
            ),
            (
                vec!["daemon", "stop", "--help"],
                vec!["Usage: loc daemon stop", "Stop the daemon", "--tcp-addr"],
            ),
            (
                vec!["daemon", "status", "--help"],
                vec![
                    "Usage: loc daemon status",
                    "Show daemon status",
                    "--tcp-addr",
                ],
            ),
            (
                vec!["daemon", "reload", "--help"],
                vec!["Usage: loc daemon reload", "Reload daemon", "--tcp-addr"],
            ),
            (
                vec!["daemon", "restart", "--help"],
                vec![
                    "Usage: loc daemon restart",
                    "Restart the daemon",
                    "--tcp-addr",
                ],
            ),
            (
                vec!["mount", "--help"],
                vec![
                    "Usage: loc mount",
                    "Commands:",
                    "notion",
                    "google-docs",
                    "--json",
                ],
            ),
            (
                vec!["mount", "notion", "--help"],
                vec![
                    "Usage: loc mount notion",
                    "Mount Notion content",
                    "--workspace",
                    "--root-page",
                ],
            ),
            (
                vec!["mount", "google-docs", "--help"],
                vec![
                    "Usage: loc mount google-docs",
                    "Mount Google Docs content",
                    "--workspace-folder",
                ],
            ),
            (
                vec!["info", "--help"],
                vec!["Usage: loc info", "Show source", "path", "--json"],
            ),
            (
                vec!["status", "--help"],
                vec![
                    "Usage: loc status",
                    "Show local sync state",
                    "path",
                    "--json",
                ],
            ),
            (
                vec!["doctor", "--help"],
                vec!["Usage: loc doctor", "Run read-only diagnostics", "--json"],
            ),
            (
                vec!["search", "--help"],
                vec![
                    "Usage: loc search",
                    "Search local mount metadata",
                    "--connector",
                    "--limit",
                ],
            ),
            (
                vec!["create", "--help"],
                vec!["Usage: loc create", "Commands:", "page", "--json"],
            ),
            (
                vec!["create", "page", "--help"],
                vec![
                    "Usage: loc create page",
                    "Create a page directory",
                    "--title",
                    "--parent",
                ],
            ),
            (
                vec!["templates", "--help"],
                vec![
                    "Usage: loc templates",
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
                    "Usage: loc templates new",
                    "Create a local workspace",
                    "--force",
                ],
            ),
            (
                vec!["templates", "apply", "--help"],
                vec!["Usage: loc templates apply", "--to", "--title", "--force"],
            ),
            (
                vec!["pull", "--help"],
                vec!["Usage: loc pull", "Pull remote content", "path", "--json"],
            ),
            (
                vec!["push", "--help"],
                vec![
                    "Usage: loc push",
                    "Push local changes",
                    "--yes",
                    "--confirm",
                ],
            ),
            (
                vec!["diff", "--help"],
                vec!["Usage: loc diff", "Preview the push plan", "path", "--json"],
            ),
            (
                vec!["undo", "--help"],
                vec![
                    "Usage: loc undo",
                    "Undo a reconciled push",
                    "push-id",
                    "--json",
                ],
            ),
            (
                vec!["log", "--help"],
                vec!["Usage: loc log", "List push journal", "path", "--json"],
            ),
            (
                vec!["restore", "--help"],
                vec![
                    "Usage: loc restore",
                    "Restore a local file",
                    "--force",
                    "--json",
                ],
            ),
            (
                vec!["config", "--help"],
                vec!["Usage: loc config", "Configuration commands", "--json"],
            ),
            (
                vec!["file-provider", "--help"],
                vec![
                    "Usage: loc file-provider",
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
                    "Usage: loc file-provider register",
                    "Register a virtual filesystem",
                    "mount-id-or-path",
                    "--json",
                ],
            ),
            (
                vec!["file-provider", "start", "--help"],
                vec![
                    "Usage: loc file-provider start",
                    "Start the background provider",
                    "mount-id-or-path",
                ],
            ),
            (
                vec!["file-provider", "stop", "--help"],
                vec![
                    "Usage: loc file-provider stop",
                    "Stop the background provider",
                    "mount-id-or-path",
                ],
            ),
            (
                vec!["file-provider", "status", "--help"],
                vec![
                    "Usage: loc file-provider status",
                    "Show provider runtime status",
                    "mount-id-or-path",
                ],
            ),
            (
                vec!["file-provider", "restart", "--help"],
                vec![
                    "Usage: loc file-provider restart",
                    "Restart the background provider",
                    "mount-id-or-path",
                ],
            ),
            (
                vec!["file-provider", "open", "--help"],
                vec![
                    "Usage: loc file-provider open",
                    "Open a registered virtual filesystem",
                    "mount-id-or-path",
                ],
            ),
            (
                vec!["file-provider", "unregister", "--help"],
                vec![
                    "Usage: loc file-provider unregister",
                    "Unregister a virtual filesystem",
                    "mount-id-or-path",
                ],
            ),
            (
                vec!["file-provider", "list", "--help"],
                vec![
                    "Usage: loc file-provider list",
                    "List registered file provider",
                ],
            ),
            (
                vec!["file-provider", "reset", "--help"],
                vec!["Usage: loc file-provider reset", "Reset file provider"],
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

        assert!(help.contains("Usage: loc push"));
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
            "/tmp/loc-state",
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
                "/tmp/loc-state",
                "--include-env",
                "NOTION_TOKEN"
            ]
        );

        let cli = parse_cli([
            "connect",
            "google-docs",
            "--name",
            "docs-work",
            "--no-browser",
            "--broker-url",
            "https://auth.example.test",
            "--redirect-uri",
            "http://localhost:8757/oauth/google-docs/callback",
        ]);
        assert_eq!(
            legacy_args_for_command(cli.command.as_ref().expect("command")),
            vec![
                "connect",
                "google-docs",
                "--name",
                "docs-work",
                "--no-browser",
                "--broker-url",
                "https://auth.example.test",
                "--redirect-uri",
                "http://localhost:8757/oauth/google-docs/callback"
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
            "create",
            "page",
            "--title",
            "Launch Plan",
            "--parent",
            "/tmp/locality/notion",
        ]);
        assert_eq!(
            legacy_args_for_command(cli.command.as_ref().expect("command")),
            vec![
                "create",
                "page",
                "--title",
                "Launch Plan",
                "--parent",
                "/tmp/locality/notion"
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
            MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/loc/notion")
                .projection(ProjectionMode::MacosFileProvider);
        let linux_mount =
            MountConfig::new(MountId::new("notion-linux"), "notion", "/tmp/loc/linux")
                .projection(ProjectionMode::LinuxFuse);
        let windows_mount = MountConfig::new(
            MountId::new("notion-windows"),
            "notion",
            r"C:\Users\Ada\Locality",
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
    fn linux_fuse_unregister_guard_blocks_shared_root_siblings() {
        let target = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion",
        )
        .projection(ProjectionMode::LinuxFuse);
        let sibling = MountConfig::new(
            MountId::new("docs-main"),
            "google-docs",
            "/tmp/Locality/docs",
        )
        .projection(ProjectionMode::LinuxFuse);
        let mounts = vec![target.clone(), sibling];

        let error = guard_linux_fuse_shared_root_unregister(&mounts, &target)
            .expect_err("shared root sibling should block unregister");

        assert_eq!(error.code, "linux_fuse_shared_root_in_use");
        assert!(error.message.contains("/tmp/Locality"));
        assert!(error.message.contains("docs-main"));
    }

    #[test]
    fn linux_fuse_unregister_guard_ignores_non_siblings() {
        let target = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion",
        )
        .projection(ProjectionMode::LinuxFuse);
        let different_root =
            MountConfig::new(MountId::new("docs-main"), "google-docs", "/tmp/Other/docs")
                .projection(ProjectionMode::LinuxFuse);
        let different_projection =
            MountConfig::new(MountId::new("plain"), "notion", "/tmp/Locality/plain")
                .projection(ProjectionMode::PlainFiles);
        let same_mount_id = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-copy",
        )
        .projection(ProjectionMode::LinuxFuse);
        let mounts = vec![
            target.clone(),
            different_root,
            different_projection,
            same_mount_id,
        ];

        guard_linux_fuse_shared_root_unregister(&mounts, &target)
            .expect("non-siblings should not block unregister");
    }

    #[test]
    fn unresolved_linux_fuse_unregister_is_blocked_when_shared_mounts_exist() {
        let mounts = vec![
            MountConfig::new(
                MountId::new("notion-main"),
                "notion",
                "/tmp/Locality/notion",
            )
            .projection(ProjectionMode::LinuxFuse),
            MountConfig::new(
                MountId::new("docs-main"),
                "google-docs",
                "/tmp/Locality/docs",
            )
            .projection(ProjectionMode::LinuxFuse),
        ];

        let error = guard_unresolved_linux_fuse_unregister(&mounts, "root-tmp-Locality")
            .expect_err("unresolved raw target should not unregister shared roots");

        assert_eq!(error.code, "linux_fuse_unresolved_shared_root");
        assert!(error.message.contains("root-tmp-Locality"));
        assert!(error.message.contains("notion-main"));
        assert!(error.message.contains("docs-main"));
    }

    #[test]
    fn windows_cloud_files_unregister_guard_blocks_shared_root_siblings() {
        let target = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion",
        )
        .projection(ProjectionMode::WindowsCloudFiles);
        let sibling = MountConfig::new(
            MountId::new("docs-main"),
            "google-docs",
            "/tmp/Locality/docs",
        )
        .projection(ProjectionMode::WindowsCloudFiles);
        let mounts = vec![target.clone(), sibling];

        let error = guard_windows_cloud_files_shared_root_unregister(&mounts, &target)
            .expect_err("shared root sibling should block unregister");

        assert_eq!(error.code, "windows_cloud_files_shared_root_in_use");
        assert!(error.message.contains("/tmp/Locality"));
        assert!(error.message.contains("docs-main"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn file_provider_mount_target_resolution_accepts_windows_case_variants() {
        let temp_root = unique_temp_path("loc-file-provider-target-resolution");
        let state_root = temp_root.join("state");
        let mount_root = temp_root.join("LocalityCase").join("notion-main");
        fs::create_dir_all(&mount_root).expect("create mount root");
        let stored_root = PathBuf::from(mount_root.display().to_string().to_ascii_lowercase());
        let mut store = SqliteStateStore::open(state_root).expect("open state");
        store
            .save_mount(
                MountConfig::new(MountId::new("notion-main"), "notion", &stored_root)
                    .projection(ProjectionMode::WindowsCloudFiles),
            )
            .expect("save mount with differently cased root");

        let resolved = resolve_mount_target(&store, &mount_root.display().to_string())
            .expect("resolve target by equivalent Windows path");

        assert_eq!(resolved.mount_id, MountId::new("notion-main"));
        let _ = fs::remove_dir_all(temp_root);
    }

    #[cfg(target_os = "windows")]
    fn unique_temp_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn unresolved_windows_cloud_files_unregister_is_blocked_when_shared_mounts_exist() {
        let mounts = vec![
            MountConfig::new(
                MountId::new("notion-main"),
                "notion",
                "/tmp/Locality/notion",
            )
            .projection(ProjectionMode::WindowsCloudFiles),
            MountConfig::new(
                MountId::new("docs-main"),
                "google-docs",
                "/tmp/Locality/docs",
            )
            .projection(ProjectionMode::WindowsCloudFiles),
        ];

        let error = guard_unresolved_windows_cloud_files_unregister(&mounts, "notoin-main")
            .expect_err("unresolved raw target should not unregister shared roots");

        assert_eq!(error.code, "windows_cloud_files_unresolved_shared_root");
        assert!(error.message.contains("notoin-main"));
        assert!(error.message.contains("notion-main"));
        assert!(error.message.contains("docs-main"));
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
    fn google_docs_default_mount_id_derives_from_connection_when_default_is_other_workspace() {
        let descriptor = crate::connector::source_descriptor(GOOGLE_DOCS_CONNECTOR_ID);
        let connection_id = ConnectionId::new("google-docs-work");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(
                    MountId::new("google-docs-main"),
                    GOOGLE_DOCS_CONNECTOR_ID,
                    "/tmp/Locality/google-docs-main",
                )
                .with_connection_id(connection_id.clone())
                .with_remote_root_id(RemoteId::new("workspace-folder-a"))
                .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save existing Google Docs mount");

        let mount_id = default_mount_id_for_source(
            &store,
            &descriptor,
            Some(&connection_id),
            Some(&RemoteId::new("workspace-folder-b")),
        )
        .expect("derive Google Docs mount id");

        assert_eq!(mount_id, MountId::new("google-docs-work"));
    }

    #[test]
    fn google_docs_default_mount_id_reuses_default_for_same_workspace() {
        let descriptor = crate::connector::source_descriptor(GOOGLE_DOCS_CONNECTOR_ID);
        let connection_id = ConnectionId::new("google-docs-work");
        let remote_root_id = RemoteId::new("workspace-folder-a");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount(
                MountConfig::new(
                    MountId::new("google-docs-main"),
                    GOOGLE_DOCS_CONNECTOR_ID,
                    "/tmp/Locality/google-docs-main",
                )
                .with_connection_id(connection_id.clone())
                .with_remote_root_id(remote_root_id.clone())
                .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save existing Google Docs mount");

        let mount_id = default_mount_id_for_source(
            &store,
            &descriptor,
            Some(&connection_id),
            Some(&remote_root_id),
        )
        .expect("derive Google Docs mount id");

        assert_eq!(mount_id, MountId::new("google-docs-main"));
    }

    #[test]
    fn linux_fuse_registration_preflight_requires_running_daemon_unless_disabled() {
        let error =
            mounted_projection_preflight_error(ProjectionMode::LinuxFuse, "linux", false, || false)
                .expect("linux fuse should require daemon");

        assert_eq!(error.code, "daemon_not_running");
        assert!(error.message.contains("localityd is not running"));
        assert!(
            error
                .suggested_command
                .as_deref()
                .is_some_and(|command| command.contains("loc daemon start"))
        );

        assert!(
            mounted_projection_preflight_error(ProjectionMode::LinuxFuse, "linux", false, || true)
                .is_none()
        );
        assert!(
            mounted_projection_preflight_error(ProjectionMode::LinuxFuse, "linux", true, || {
                panic!("daemon check should be skipped when daemon-disabled")
            })
            .is_none()
        );
        assert!(
            mounted_projection_preflight_error(ProjectionMode::PlainFiles, "linux", false, || {
                panic!("daemon check should be skipped for plain files")
            })
            .is_none()
        );
    }

    #[test]
    fn file_provider_list_lines_print_linux_roots_and_stale_units() {
        let report = FileProviderCommandReport {
            ok: true,
            command: "file-provider",
            action: "list".to_string(),
            mount_id: None,
            helper: "systemctl".to_string(),
            helper_report: serde_json::json!({
                "message": "Linux FUSE roots listed",
                "roots": [
                    {
                        "service": "ai.codeflash.locality.fuse.root-home.service",
                        "mount_ids": ["google-docs-main", "notion-main"],
                        "mountpoint": "/home/example/Locality",
                        "registered": true,
                        "active": false
                    }
                ],
                "stale_units": [
                    {
                        "service": "ai.codeflash.locality.fuse.notion-main.service",
                        "mountpoint": "/home/example/Locality/notion-main",
                        "unit_path": "/home/example/.config/systemd/user/ai.codeflash.locality.fuse.notion-main.service",
                        "legacy": true
                    }
                ]
            }),
        };

        assert_eq!(
            file_provider_list_lines(&report),
            vec![
                "linux-fuse\tgoogle-docs-main,notion-main\t/home/example/Locality\tregistered=true\tactive=false".to_string(),
                "stale-linux-fuse\tai.codeflash.locality.fuse.notion-main.service\t/home/example/Locality/notion-main\t/home/example/.config/systemd/user/ai.codeflash.locality.fuse.notion-main.service\tlegacy=true".to_string(),
            ]
        );
    }

    #[test]
    fn command_paths_absolutize_relative_state_roots() {
        let path = absolute_command_path(Path::new(".loc"));

        assert!(path.is_absolute());
        assert!(path.ends_with(".loc"));
    }

    #[test]
    fn pull_direct_fallback_refuses_timeout_and_virtual_mount_without_daemon() {
        let virtual_mount =
            MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/loc/notion")
                .projection(ProjectionMode::LinuxFuse);
        let plain_mount = MountConfig::new(MountId::new("plain"), "notion", "/tmp/loc/plain")
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
    fn local_redirect_defaults_to_loopback_callback_uri() {
        let redirect =
            local_redirect("http://localhost:8757/oauth/notion/callback").expect("redirect");

        assert_eq!(redirect.bind_addr, "127.0.0.1:8757");
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

    #[test]
    fn google_docs_oauth_broker_config_accepts_explicit_broker_url() {
        let args = vec![
            "google-docs".to_string(),
            "--broker-url".to_string(),
            "https://auth.example.test".to_string(),
            "--redirect-uri".to_string(),
            "http://localhost:8757/oauth/google-docs/callback".to_string(),
        ];

        let config = google_docs_oauth_broker_config(&args).expect("broker config");

        assert_eq!(config.broker_url, "https://auth.example.test");
        assert_eq!(
            config.redirect_uri,
            "http://localhost:8757/oauth/google-docs/callback"
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
        std::iter::once("loc".to_string())
            .chain(args.into_iter().map(|arg| arg.as_ref().to_string()))
            .collect()
    }
}
