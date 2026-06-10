use std::path::PathBuf;

use afs_store::SqliteStateStore;
use serde::Serialize;

use crate::diff::{DiffError, run_diff};

const COMMANDS: &[&str] = &[
    "connect", "mount", "status", "pull", "push", "diff", "undo", "log", "resolve", "config",
];

pub fn dispatch(args: &[String]) -> i32 {
    if args.is_empty() || has_flag(args, "--help") || has_flag(args, "-h") {
        print_help();
        return 0;
    }

    let json = has_flag(args, "--json");
    match args[0].as_str() {
        "connect" => stub("connect", json),
        "mount" => stub("mount", json),
        "status" => stub("status", json),
        "pull" => stub("pull", json),
        "push" => stub("push", json),
        "diff" => diff(&args[1..], json),
        "undo" => stub("undo", json),
        "log" => stub("log", json),
        "resolve" => stub("resolve", json),
        "config" => stub("config", json),
        command => {
            eprintln!("unknown command: {command}");
            print_help();
            2
        }
    }
}

fn diff(args: &[String], json: bool) -> i32 {
    let Some(path) = first_positional(args) else {
        return command_error(
            json,
            CommandError::new("diff", "usage", "usage: afs diff <path> [--json]"),
            2,
        );
    };

    let store = match SqliteStateStore::open(default_state_root()) {
        Ok(store) => store,
        Err(error) => {
            return command_error(
                json,
                CommandError::new("diff", "store_open_failed", error.to_string()),
                1,
            );
        }
    };

    match run_diff(&store, PathBuf::from(path)) {
        Ok(report) if json => print_json(&report),
        Ok(report) => print_diff_report(&report),
        Err(error) => {
            let exit_code = diff_error_exit_code(&error);
            return command_error(
                json,
                CommandError::new("diff", error.code(), error.message()),
                exit_code,
            );
        }
    }

    0
}

fn stub(command: &str, json: bool) -> i32 {
    if json {
        println!("{{\"ok\":false,\"command\":\"{command}\",\"error\":\"not_implemented\"}}");
    } else {
        println!("afs {command}: not implemented yet");
    }

    0
}

fn print_diff_report(report: &crate::diff::DiffReport) {
    if !report.validation.is_empty() {
        for issue in &report.validation {
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

    let Some(plan) = &report.plan else {
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
        DiffError::MountNotFound(_) => 2,
        DiffError::ReadFile { .. } => 1,
        DiffError::Store(_) => 1,
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
