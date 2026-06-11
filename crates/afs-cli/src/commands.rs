use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use afs_connector::ConnectorUndoApplier;
use afs_core::AfsError;
use afs_core::model::{MountId, RemoteId};
use afs_notion::{NotionConfig, NotionConnector};
use afs_store::{MountConfig, MountRepository, ProjectionMode, SqliteStateStore};
use afsd::execution::PushJobReport;
use afsd::ipc::{DaemonClientError, DaemonRequest, send_request};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::daemon::{DaemonControlError, DaemonControlReport, run_daemon_control};
use crate::diff::{DiffError, run_diff};
use crate::history::{
    HistoryError, LogOptions, LogReport, UndoReport, run_log, run_undo_with_applier,
    undo_report_exit_code,
};
use crate::info::{InfoError, InfoOptions, InfoReport, run_info};
use crate::mount::{MountError, MountOptions, MountReport, run_mount};
use crate::pull::{PullError, PullReport, run_pull};
use crate::push::{PushOptions, PushReport, push_report_exit_code, run_push_with_daemon};
use crate::restore::{RestoreError, RestoreOptions, RestoreReport, run_restore};
use crate::status::{StatusError, StatusOptions, StatusReport, run_status};

const EXIT_SUCCESS: i32 = 0;
const EXIT_INTERNAL: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_VALIDATION: i32 = 3;

const COMMANDS: &[&str] = &[
    "connect",
    "mount",
    "info",
    "status",
    "pull",
    "push",
    "daemon",
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
        "connect" => stub("connect", json),
        "mount" => mount(&args[1..], json),
        "info" => info(&args[1..], json),
        "status" => status(&args[1..], json),
        "pull" => pull(&args[1..], json),
        "push" => push(&args[1..], json),
        "daemon" => daemon(&args[1..], json),
        "diff" => diff(&args[1..], json),
        "restore" => restore(&args[1..], json),
        "undo" => undo(&args[1..], json),
        "log" => log(&args[1..], json),
        "resolve" => stub("resolve", json),
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

fn file_provider(args: &[String], json: bool) -> i32 {
    let Some(action) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new(
                "file-provider",
                "usage",
                "usage: afs file-provider register|unregister <mount-id-or-path> [--json]",
            ),
            EXIT_USAGE,
        );
    };

    match action {
        "register" => file_provider_register(args, json),
        "unregister" => file_provider_unregister(args, json),
        "list" => run_file_provider_helper(json, "list", Vec::new(), None),
        "reset" => run_file_provider_helper(json, "reset", Vec::new(), None),
        _ => command_error(
            json,
            CommandError::new(
                "file-provider",
                "usage",
                "usage: afs file-provider register|unregister|list|reset",
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
    if mount.projection != ProjectionMode::MacosFileProvider {
        return command_error(
            json,
            CommandError::new(
                "file-provider",
                "wrong_projection",
                format!(
                    "mount `{}` uses projection `{}`; remount with --projection macos-file-provider",
                    mount.mount_id.0,
                    mount.projection.as_str()
                ),
            ),
            EXIT_USAGE,
        );
    }

    let mount_id = mount.mount_id.0.clone();
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

    let mount_id = match SqliteStateStore::open(default_state_root())
        .ok()
        .and_then(|store| resolve_mount_target(&store, target).ok())
    {
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

    let mut store = match SqliteStateStore::open(default_state_root()) {
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

fn mount(args: &[String], json: bool) -> i32 {
    if first_positional(args) != Some("notion") {
        return command_error(
            json,
            CommandError::new(
                "mount",
                "usage",
                "usage: afs mount notion <path> --root-page <page-id> [--mount-id <id>] [--projection plain-files|macos-file-provider] [--read-only] [--json]",
            ),
            EXIT_USAGE,
        );
    }

    let Some(root) = nth_positional(args, 1) else {
        return command_error(
            json,
            CommandError::new(
                "mount",
                "usage",
                "usage: afs mount notion <path> --root-page <page-id> [--mount-id <id>] [--projection plain-files|macos-file-provider] [--read-only] [--json]",
            ),
            EXIT_USAGE,
        );
    };
    let Some(root_page_id) = flag_value(args, "--root-page") else {
        return command_error(
            json,
            CommandError::new(
                "mount",
                "usage",
                "afs mount notion requires --root-page <page-id>",
            ),
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

    let mut store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("mount", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };

    let options = MountOptions {
        mount_id: MountId::new(
            flag_value(args, "--mount-id")
                .map(str::to_string)
                .unwrap_or_else(|| "notion-main".to_string()),
        ),
        connector: "notion".to_string(),
        root: PathBuf::from(root),
        remote_root_id: Some(RemoteId::new(root_page_id)),
        read_only: has_flag(args, "--read-only"),
        projection,
    };

    match run_mount(&mut store, options) {
        Ok(report) if json => {
            print_json(&report);
            EXIT_SUCCESS
        }
        Ok(report) => {
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
    match run_daemon_report::<PullReport>(
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
        DaemonReport::Unavailable => {}
        DaemonReport::Error(error) => {
            return command_error(
                json,
                CommandError::new("pull", error.code, error.message),
                error.exit_code,
            );
        }
    }

    let mut store = match SqliteStateStore::open(state_root) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("pull", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };
    let connector = default_notion_connector();

    match run_pull(&mut store, &connector, PathBuf::from(path)) {
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

    let connector = default_notion_connector();
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
        DaemonReport::Unavailable => {}
        DaemonReport::Error(error) => {
            return command_error(
                json,
                CommandError::new("push", error.code, error.message),
                error.exit_code,
            );
        }
    }

    let mut store = match SqliteStateStore::open(state_root) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("push", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };

    let connector = default_notion_connector();

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
            "push {} reconciled",
            report.push_id.as_deref().unwrap_or("<unknown>")
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
    println!(
        "agent guidance: {} {}, {} {}",
        report.guidance.agents_md.action.as_str(),
        report.guidance.agents_md.path,
        report.guidance.claude_md.action.as_str(),
        report.guidance.claude_md.path
    );
}

fn print_pull_report(report: &PullReport) {
    if report.skipped_dirty > 0 {
        println!(
            "pull skipped {} dirty file(s); {} hydrated, {} stubbed, {} enumerated",
            report.skipped_dirty, report.hydrated, report.stubbed, report.enumerated
        );
    } else {
        println!(
            "pull complete: {} hydrated, {} stubbed, {} enumerated",
            report.hydrated, report.stubbed, report.enumerated
        );
    }
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
    let helper = match file_provider_helper_path() {
        Some(path) => path,
        None => {
            return command_error(
                json,
                CommandError::new(
                    "file-provider",
                    "helper_missing",
                    "agentfs-file-providerctl was not found; build or install platform/macos/AgentFSFileProvider first",
                ),
                EXIT_INTERNAL,
            );
        }
    };

    let mut command = ProcessCommand::new(&helper);
    command.arg(action);
    command.args(args);
    command.arg("--json");
    let output = match command.output() {
        Ok(output) => output,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("file-provider", "helper_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };

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
            .unwrap_or_else(|| format!("agentfs-file-providerctl exited with {}", output.status));
        return command_error(
            json,
            CommandError::new("file-provider", "helper_failed", message),
            EXIT_INTERNAL,
        );
    }

    let report = FileProviderCommandReport {
        ok: true,
        command: "file-provider",
        action: action.to_string(),
        mount_id,
        helper: helper.display().to_string(),
        helper_report,
    };

    if json {
        print_json(&report);
    } else {
        print_file_provider_report(&report);
    }
    EXIT_SUCCESS
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

fn file_provider_helper_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("AFS_FILE_PROVIDERCTL") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        candidates.push(dir.join("agentfs-file-providerctl"));
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let package_dir = manifest_dir.join("../../platform/macos/AgentFSFileProvider");
    candidates.push(
        package_dir.join(".build/dev-bundle/AgentFS.app/Contents/MacOS/agentfs-file-providerctl"),
    );
    candidates.push(PathBuf::from(
        "/Applications/AgentFS.app/Contents/MacOS/agentfs-file-providerctl",
    ));
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join("Applications/AgentFS.app/Contents/MacOS/agentfs-file-providerctl"),
        );
    }
    candidates.push(package_dir.join(".build/debug/agentfs-file-providerctl"));
    candidates.push(package_dir.join(".build/release/agentfs-file-providerctl"));

    candidates.into_iter().find(|path| path.exists())
}

fn default_notion_connector() -> NotionConnector {
    NotionConnector::new(NotionConfig::default())
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

enum DaemonReport<T> {
    Report(T),
    Unavailable,
    Error(DaemonCommandError),
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
        return DaemonReport::Unavailable;
    }

    let response = match send_request(state_root, request) {
        Ok(response) => response,
        Err(DaemonClientError::NotAvailable(_)) => return DaemonReport::Unavailable,
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

fn daemon_error_exit_code(code: &str) -> i32 {
    match code {
        "mount_not_found" | "entity_path_missing" => EXIT_USAGE,
        "validation_failed" => EXIT_VALIDATION,
        "not_implemented" => 5,
        _ => EXIT_INTERNAL,
    }
}

fn command_error(json: bool, error: CommandError, exit_code: i32) -> i32 {
    if json {
        print_json(&error);
    } else {
        eprintln!("afs {}: {}", error.command, error.message);
    }

    exit_code
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

fn projection_mode(args: &[String]) -> Result<ProjectionMode, &'static str> {
    match flag_value(args, "--projection") {
        None | Some("plain-files") => Ok(ProjectionMode::PlainFiles),
        Some("macos-file-provider") => Ok(ProjectionMode::MacosFileProvider),
        Some(_) => Err("--projection must be plain-files or macos-file-provider"),
    }
}

fn takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "--root-page" | "--mount-id" | "--projection" | "--helper" | "--display-name"
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

#[derive(Serialize)]
struct CommandError {
    ok: bool,
    command: &'static str,
    code: String,
    message: String,
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
        }
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
    use crate::diff::{DiffReport, GuardrailOutput};
    use crate::push::PushReport;

    use super::{EXIT_SUCCESS, EXIT_VALIDATION, diff_report_exit_code};

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
