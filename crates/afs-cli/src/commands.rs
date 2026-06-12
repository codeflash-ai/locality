use std::io::{self, Read};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command as ProcessCommand;
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
use afsd::ipc::{DaemonClientError, DaemonRequest, send_request_with_timeout};
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
    ConnectorResolveError, resolve_notion_connector_for_mount_id, resolve_notion_connector_for_path,
};
use crate::daemon::{DaemonControlError, DaemonControlReport, run_daemon_control};
use crate::diff::{DiffError, run_diff};
use crate::file_provider as file_provider_helper;
use crate::history::{
    HistoryError, LogOptions, LogReport, UndoReport, run_log, run_undo_with_applier,
    undo_report_exit_code,
};
use crate::info::{InfoError, InfoOptions, InfoReport, run_info};
use crate::local_oauth::{
    LocalOAuthAuthorization, LocalOAuthError, local_redirect, notion_authorize_url, random_state,
    run_local_oauth_authorization,
};
use crate::mount::{MountError, MountOptions, MountReport, run_mount};
use crate::pull::{PullError, PullReport, run_pull_with_state_root};
use crate::push::{PushOptions, PushReport, push_report_exit_code, run_push_with_daemon};
use crate::resolve::{ResolveChoice, ResolveError, ResolveOptions, ResolveReport, run_resolve};
use crate::restore::{RestoreError, RestoreOptions, RestoreReport, run_restore};
use crate::status::{StatusError, StatusOptions, StatusReport, run_status};

const EXIT_SUCCESS: i32 = 0;
const EXIT_INTERNAL: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_VALIDATION: i32 = 3;
const DEFAULT_DAEMON_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_DAEMON_MUTATING_TIMEOUT: Duration = Duration::from_secs(60);

const COMMANDS: &[&str] = &[
    "connect",
    "connections",
    "profiles",
    "connection",
    "disconnect",
    "daemon",
    "mount",
    "info",
    "status",
    "pull",
    "push",
    "diff",
    "undo",
    "log",
    "resolve",
    "restore",
    "config",
    "file-provider",
];

pub fn dispatch(args: &[String]) -> i32 {
    if args.is_empty() || has_flag(args, "--help") || has_flag(args, "-h") {
        print_help();
        return EXIT_SUCCESS;
    }

    let json = has_flag(args, "--json");
    match args[0].as_str() {
        "connect" => connect(&args[1..], json),
        "connections" => connections(&args[1..], json),
        "profiles" => profiles(&args[1..], json),
        "connection" => connection(&args[1..], json),
        "disconnect" => disconnect(&args[1..], json),
        "daemon" => daemon(&args[1..], json),
        "mount" => mount(&args[1..], json),
        "info" => info(&args[1..], json),
        "status" => status(&args[1..], json),
        "pull" => pull(&args[1..], json),
        "push" => push(&args[1..], json),
        "diff" => diff(&args[1..], json),
        "restore" => restore(&args[1..], json),
        "resolve" => resolve(&args[1..], json),
        "undo" => undo(&args[1..], json),
        "log" => log(&args[1..], json),
        "config" => stub("config", json),
        "file-provider" => file_provider(&args[1..], json),
        command => {
            eprintln!("unknown command: {command}");
            print_help();
            EXIT_USAGE
        }
    }
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
        let authorization = match run_local_oauth_authorization(
            "Notion",
            &start.authorization_url,
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
                "usage: afs file-provider register|open|unregister <mount-id-or-path> [--json]",
            ),
            EXIT_USAGE,
        );
    };

    match action {
        "register" => file_provider_register(args, json),
        "open" => file_provider_open(args, json),
        "unregister" => file_provider_unregister(args, json),
        "list" => run_file_provider_helper(json, "list", Vec::new(), None),
        "reset" => run_file_provider_helper(json, "reset", Vec::new(), None),
        _ => command_error(
            json,
            CommandError::new(
                "file-provider",
                "usage",
                "usage: afs file-provider register|open|unregister|list|reset",
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

fn resolve(args: &[String], json: bool) -> i32 {
    let choice = if has_flag(args, "--ours") {
        Some(ResolveChoice::Ours)
    } else if has_flag(args, "--theirs") {
        Some(ResolveChoice::Theirs)
    } else if has_flag(args, "--edited") {
        Some(ResolveChoice::Edited)
    } else {
        None
    };
    let Some(choice) = choice else {
        return command_error(
            json,
            CommandError::new(
                "resolve",
                "usage",
                "usage: afs resolve --ours|--theirs|--edited <path> [--json]",
            ),
            EXIT_USAGE,
        );
    };
    let Some(path) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new(
                "resolve",
                "usage",
                "usage: afs resolve --ours|--theirs|--edited <path> [--json]",
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
                CommandError::new("resolve", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };

    match run_resolve(&mut store, PathBuf::from(path), ResolveOptions { choice }) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            print_resolve_report(&report);
            EXIT_SUCCESS
        }
        Err(error) => resolve_command_error(json, error),
    }
}

fn mount(args: &[String], json: bool) -> i32 {
    if first_positional(args) != Some("notion") {
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
                "afs mount notion accepts either --workspace or --root-page <page-id>, not both",
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
                "afs mount notion requires --workspace or --root-page <page-id>",
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
                .unwrap_or_else(|| "notion-main".to_string()),
        ),
        connector: "notion".to_string(),
        root: PathBuf::from(root),
        remote_root_id: root_page_id.map(RemoteId::new),
        connection_id,
        read_only: has_flag(args, "--read-only"),
        projection,
    };

    match run_mount(&mut store, options) {
        Ok(report) if json => {
            notify_daemon_mounts_changed(&state_root);
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
            notify_daemon_mounts_changed(&state_root);
            print_mount_report(&report);
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
    let fallback_reason = match run_daemon_report::<PullReport>(
        &state_root,
        &DaemonRequest::Pull {
            path: PathBuf::from(path),
        },
    ) {
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
    let connector = match resolve_notion_connector_for_path(&store, credentials.as_ref(), path) {
        Ok(connector) => connector,
        Err(error) => return connector_command_error("pull", json, error),
    };

    match run_pull_with_state_root(
        &mut store,
        &connector,
        PathBuf::from(path),
        Some(&state_root),
    ) {
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
    let store = match SqliteStateStore::open(state_root.clone()) {
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
    let connector = match resolve_notion_connector_for_mount_id(
        &store,
        credentials.as_ref(),
        &journal.mount_id,
    ) {
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

    match run_daemon_report::<PushJobReport>(
        &state_root,
        &DaemonRequest::Push {
            path: PathBuf::from(path),
            assume_yes: options.assume_yes,
            confirm_dangerous: options.confirm_dangerous,
        },
    ) {
        DaemonReport::Report(report) if json => {
            let report = PushReport::from_daemon(report);
            let exit_code = push_report_exit_code(&report);
            print_json(&report);
            return exit_code;
        }
        DaemonReport::Report(report) => {
            let report = PushReport::from_daemon(report);
            let exit_code = push_report_exit_code(&report);
            print_push_report(&report);
            return exit_code;
        }
        DaemonReport::Unavailable(DaemonUnavailableReason::TimedOut) => {
            return command_error(
                json,
                CommandError::new(
                    "push",
                    "daemon_timeout",
                    format!(
                        "afsd did not respond within {}ms after the push request was submitted; refusing direct fallback to avoid duplicate remote writes",
                        daemon_mutating_request_timeout().as_millis()
                    ),
                ),
                EXIT_INTERNAL,
            );
        }
        DaemonReport::Unavailable(reason) => warn_daemon_fallback("push", reason),
        DaemonReport::Error(error) => {
            return command_error(
                json,
                CommandError::new("push", error.code, error.message),
                error.exit_code,
            );
        }
    }

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

    let credentials = open_credential_store(&state_root);
    let connector = match resolve_notion_connector_for_path(&store, credentials.as_ref(), path) {
        Ok(connector) => connector,
        Err(error) => return connector_command_error("push", json, error),
    };

    match run_push_with_daemon(&mut store, &connector, PathBuf::from(path), options) {
        Ok(report) if json => {
            let exit_code = push_report_exit_code(&report);
            print_json(&report);
            exit_code
        }
        Ok(report) => {
            let exit_code = push_report_exit_code(&report);
            print_push_report(&report);
            exit_code
        }
        Err(error) => {
            let exit_code = afs_error_exit_code(&error);
            command_error(
                json,
                CommandError::new("push", afs_error_code(&error), error.to_string()),
                exit_code,
            )
        }
    }
}

fn diff(args: &[String], json: bool) -> i32 {
    let Some(path) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new("diff", "usage", "usage: afs diff <path> [--json]"),
            EXIT_USAGE,
        );
    };

    let store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("diff", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };

    match run_diff(&store, PathBuf::from(path)) {
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
            "  summary: {} updated, {} created, {} moved, {} archived",
            entry.plan_summary.blocks_updated,
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
    if report.skipped_dirty > 0 {
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

fn print_restore_report(report: &RestoreReport) {
    println!("restored {}", report.path);
}

fn print_resolve_report(report: &ResolveReport) {
    println!("{}", report.message);
}

fn print_status_report(report: &StatusReport) {
    if report.mounts.is_empty() {
        println!("no mounts");
        return;
    }

    let mut printed_entries = 0;
    for mount in &report.mounts {
        for entry in &mount.entries {
            if entry.state.as_str() == "clean"
                && entry.pending_journal_count == 0
                && entry.failed_journal_count == 0
            {
                continue;
            }

            printed_entries += 1;
            println!("{}  {}", mount.mount_id, entry.path);
            println!(
                "  state: {}  hydration: {}",
                entry.state.as_str(),
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
        println!(
            "status clean: {} tracked entr{}",
            report.summary.total,
            if report.summary.total == 1 {
                "y"
            } else {
                "ies"
            }
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
    }
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
            println!("Remote edited: {remote_edited_at}");
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
        println!("  watched mounts: {}", status.watches.watched_mounts);
        println!(
            "  jobs: active={}, pending={}, hydration={}",
            status.runtime.active_job,
            status.runtime.pending_requests,
            status.runtime.pending_hydrations
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

fn print_file_provider_report(report: &FileProviderCommandReport) {
    if report.action == "list" {
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
    mount
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| mount.mount_id.0.clone())
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
        "{} blocks updated, {} created, {} moved, {} archived",
        plan.summary.blocks_updated,
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
                .with_suggested_command("afs connect notion")
            })?;
        if connection.status != "active" {
            return Err(CommandError::new(
                "mount",
                "connection_revoked",
                format!("connection `{}` is revoked", connection.connection_id.0),
            )
            .with_suggested_command("afs connect notion"));
        }
        validate_connection_profile(store, &connection)?;
        return Ok(Some(connection.connection_id));
    }

    let active = store
        .list_connections()
        .map_err(|error| CommandError::new("mount", "store_error", error.to_string()))?
        .into_iter()
        .filter(|connection| connection.connector == "notion" && connection.status == "active")
        .collect::<Vec<_>>();
    for connection in &active {
        validate_connection_profile(store, connection)?;
    }
    match active.as_slice() {
        [connection] => Ok(Some(connection.connection_id.clone())),
        [] if std::env::var("NOTION_TOKEN").is_ok() => Ok(None),
        [] => Err(CommandError::new(
            "mount",
            "missing_connection",
            "missing Notion connection; run `afs connect notion`",
        )
        .with_suggested_command("afs connect notion")),
        _ => Err(CommandError::new(
            "mount",
            "missing_connection",
            "multiple Notion connections exist; pass --connection <id>",
        )),
    }
}

fn validate_connection_profile(
    store: &SqliteStateStore,
    connection: &ConnectionRecord,
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
            .with_suggested_command("afs connect notion")
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
        .with_suggested_command("afs connect notion"));
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
        .with_suggested_command("afs connect notion"));
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

fn resolve_command_error(json: bool, error: ResolveError) -> i32 {
    let exit_code = match &error {
        ResolveError::MountNotFound(_)
        | ResolveError::Store(afs_store::StoreError::EntityPathMissing { .. }) => EXIT_USAGE,
        ResolveError::EntityNotConflicted(_) => 4,
        ResolveError::CurrentDir(_)
        | ResolveError::ReadFile { .. }
        | ResolveError::RemoteSidecarMissing(_)
        | ResolveError::Store(_)
        | ResolveError::UnsupportedEntity(_)
        | ResolveError::WriteFile { .. } => EXIT_INTERNAL,
    };
    command_error(
        json,
        CommandError::new("resolve", error.code(), error.message()),
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
    match flag_value(args, "--projection") {
        None | Some("plain-files") => Ok(ProjectionMode::PlainFiles),
        Some("macos-file-provider") if target_os == "macos" => {
            Ok(ProjectionMode::MacosFileProvider)
        }
        Some("linux-fuse") if target_os == "linux" => Ok(ProjectionMode::LinuxFuse),
        Some("macos-file-provider") => Err(format!(
            "--projection macos-file-provider is only supported on macOS; this binary is running on {target_os}"
        )),
        Some("linux-fuse") => Err(format!(
            "--projection linux-fuse is only supported on Linux; this binary is running on {target_os}"
        )),
        Some(_) => Err(format!(
            "--projection must be {}",
            projection_usage_options_for_target(target_os)
        )),
    }
}

fn mount_usage() -> String {
    format!(
        "usage: afs mount notion <path> (--workspace|--root-page <page-id>) [--connection <id>] [--mount-id <id>] [--projection {}] [--read-only] [--json]",
        projection_usage_options_for_target(std::env::consts::OS)
    )
}

fn projection_usage_options_for_target(target_os: &str) -> &'static str {
    match target_os {
        "macos" => "plain-files|macos-file-provider",
        "linux" => "plain-files|linux-fuse",
        _ => "plain-files",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VirtualProjectionRegistration {
    MacosFileProvider,
    LinuxFuse,
}

impl VirtualProjectionRegistration {
    fn projection(self) -> ProjectionMode {
        match self {
            Self::MacosFileProvider => ProjectionMode::MacosFileProvider,
            Self::LinuxFuse => ProjectionMode::LinuxFuse,
        }
    }

    fn projection_cli_value(self) -> &'static str {
        match self {
            Self::MacosFileProvider => "macos-file-provider",
            Self::LinuxFuse => "linux-fuse",
        }
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
    match target_os {
        "macos" => Some(VirtualProjectionRegistration::MacosFileProvider),
        "linux" => Some(VirtualProjectionRegistration::LinuxFuse),
        _ => None,
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
    )
}

fn default_state_root() -> PathBuf {
    if let Ok(value) = std::env::var("AFS_STATE_DIR") {
        return PathBuf::from(value);
    }

    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".afs");
    }

    PathBuf::from(".afs")
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
}

fn print_help() {
    println!("afs <command> [options]");
    println!();
    println!("Commands:");
    for command in COMMANDS {
        println!("  {command}");
    }
}

#[cfg(test)]
mod tests {
    use afs_core::model::MountId;
    use afs_store::{MountConfig, ProjectionMode};

    use crate::diff::{DiffReport, GuardrailOutput};
    use crate::local_oauth::{local_redirect, notion_authorize_url, parse_oauth_callback};
    use crate::push::PushReport;

    use super::{
        DaemonUnavailableReason, EXIT_SUCCESS, EXIT_VALIDATION, VirtualProjectionRegistration,
        diff_report_exit_code, notion_oauth_broker_config, projection_mode_for_target,
        projection_usage_options_for_target, pull_direct_fallback_error,
        validate_virtual_projection_registration,
    };

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
            projection_usage_options_for_target("windows"),
            "plain-files"
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

        let unsupported_platform =
            validate_virtual_projection_registration(&macos_mount, "windows")
                .expect_err("windows has no virtual projection registration yet");
        assert_eq!(unsupported_platform.code, "unsupported_platform");
        assert!(
            unsupported_platform
                .message
                .contains("no virtual filesystem registration is implemented")
        );
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
}
