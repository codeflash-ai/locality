//! Shared helpers for registering and opening platform virtual filesystem domains.
//!
//! The macOS File Provider control surface lives in the Swift helper bundled
//! with the File Provider extension. Rust entrypoints call this module rather
//! than shelling through `afs file-provider`, so the CLI and desktop app share
//! the same platform boundary.

use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct FileProviderHelperReport {
    pub helper: PathBuf,
    pub helper_report: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileProviderHelperError {
    Missing,
    Failed(String),
}

impl FileProviderHelperError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing => "helper_missing",
            Self::Failed(_) => "helper_failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Missing => {
                "agentfs-file-providerctl was not found; build or install platform/macos/AgentFSFileProvider first"
                    .to_string()
            }
            Self::Failed(message) => message.clone(),
        }
    }
}

pub fn register_macos_file_provider_domain(
    mount_id: &str,
    display_name: &str,
) -> Result<FileProviderHelperReport, FileProviderHelperError> {
    run_macos_file_provider_helper(
        "register",
        vec![
            "--mount-id".to_string(),
            mount_id.to_string(),
            "--display-name".to_string(),
            display_name.to_string(),
        ],
    )
}

pub fn open_macos_file_provider_domain(
    mount_id: &str,
) -> Result<FileProviderHelperReport, FileProviderHelperError> {
    let report = run_macos_file_provider_helper(
        "open",
        vec!["--mount-id".to_string(), mount_id.to_string()],
    )?;
    let url = report
        .helper_report
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| {
            FileProviderHelperError::Failed(
                "agentfs-file-providerctl did not return a CloudStorage URL".to_string(),
            )
        })?;
    Command::new("open")
        .arg(url)
        .spawn()
        .map_err(|error| FileProviderHelperError::Failed(error.to_string()))?;
    Ok(report)
}

pub fn run_macos_file_provider_helper(
    action: &str,
    args: Vec<String>,
) -> Result<FileProviderHelperReport, FileProviderHelperError> {
    let helper = file_provider_helper_path().ok_or(FileProviderHelperError::Missing)?;
    let mut command = Command::new(&helper);
    command.arg(action);
    command.args(args);
    command.arg("--json");

    let output = command
        .output()
        .map_err(|error| FileProviderHelperError::Failed(error.to_string()))?;
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
        return Err(FileProviderHelperError::Failed(message));
    }

    Ok(FileProviderHelperReport {
        helper,
        helper_report,
    })
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

    candidates.push(PathBuf::from(
        "/Applications/AgentFS.app/Contents/MacOS/agentfs-file-providerctl",
    ));
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join("Applications/AgentFS.app/Contents/MacOS/agentfs-file-providerctl"),
        );
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let package_dir = manifest_dir.join("../../platform/macos/AgentFSFileProvider");
    candidates.push(
        package_dir.join(".build/dev-bundle/AgentFS.app/Contents/MacOS/agentfs-file-providerctl"),
    );
    candidates.push(package_dir.join(".build/debug/agentfs-file-providerctl"));
    candidates.push(package_dir.join(".build/release/agentfs-file-providerctl"));

    candidates.into_iter().find(|path| path.exists())
}
