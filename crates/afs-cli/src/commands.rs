use std::path::PathBuf;

use afs_connector::{ConnectorPushApplier, ConnectorPushConcurrencyCheck, ConnectorUndoApplier};
use afs_notion::{NotionConfig, NotionConnector};
use afs_store::SqliteStateStore;
use serde::Serialize;

use crate::diff::{DiffError, run_diff};
use crate::history::{
    HistoryError, LogOptions, LogReport, UndoReport, run_log, run_undo_with_applier,
    undo_report_exit_code,
};
use crate::push::{
    NotImplementedReconciler, PushOptions, PushReport, push_report_exit_code,
    run_push_with_executor,
};

const EXIT_SUCCESS: i32 = 0;
const EXIT_INTERNAL: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_VALIDATION: i32 = 3;

const COMMANDS: &[&str] = &[
    "connect", "mount", "status", "pull", "push", "diff", "undo", "log", "resolve", "config",
];

pub fn dispatch(args: &[String]) -> i32 {
    if args.is_empty() || has_flag(args, "--help") || has_flag(args, "-h") {
        print_help();
        return EXIT_SUCCESS;
    }

    let json = has_flag(args, "--json");
    match args[0].as_str() {
        "connect" => stub("connect", json),
        "mount" => stub("mount", json),
        "status" => stub("status", json),
        "pull" => stub("pull", json),
        "push" => push(&args[1..], json),
        "diff" => diff(&args[1..], json),
        "undo" => undo(&args[1..], json),
        "log" => log(&args[1..], json),
        "resolve" => stub("resolve", json),
        "config" => stub("config", json),
        command => {
            eprintln!("unknown command: {command}");
            print_help();
            EXIT_USAGE
        }
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

    let mut store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("push", "store_open_failed", error.to_string()),
                EXIT_INTERNAL,
            );
        }
    };

    let options = PushOptions {
        assume_yes: has_flag(args, "-y") || has_flag(args, "--yes"),
        confirm_dangerous: has_flag(args, "--confirm"),
    };

    let connector = default_notion_connector();
    let mut concurrency = ConnectorPushConcurrencyCheck::new(&connector);
    let mut applier = ConnectorPushApplier::new(&connector);
    let mut reconciler = NotImplementedReconciler;

    match run_push_with_executor(
        &mut store,
        PathBuf::from(path),
        options,
        &mut concurrency,
        &mut applier,
        &mut reconciler,
    ) {
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
            let exit_code = diff_error_exit_code(&error);
            command_error(
                json,
                CommandError::new("push", error.code(), error.message()),
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
        _ => println!("push stopped: {}", report.action),
    }
}

fn default_notion_connector() -> NotionConnector {
    NotionConnector::new(NotionConfig {
        workspace_id: None,
        token_key: "notion-default".to_string(),
    })
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

fn diff_report_exit_code(report: &crate::diff::DiffReport) -> i32 {
    if report.ok {
        EXIT_SUCCESS
    } else {
        EXIT_VALIDATION
    }
}

fn first_positional(args: &[String]) -> Option<&str> {
    args.iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|arg| arg == flag)
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
    code: &'static str,
    message: String,
}

impl CommandError {
    fn new(command: &'static str, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            command,
            code,
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
        }
    }
}
