//! Local draft creation helpers for `loc create`.
//!
//! Creation stays filesystem-first: this module writes the draft shape that
//! push and Live Mode already understand. It does not call remote connectors.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use locality_core::path_projection::PAGE_DOCUMENT_FILENAME;
use locality_store::{MountRepository, StoreError};
use localityd::file_provider;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatePageOptions {
    pub title: String,
    pub parent: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreatePageReport {
    pub ok: bool,
    pub command: &'static str,
    pub kind: &'static str,
    pub title: String,
    pub parent: String,
    pub directory: String,
    pub path: String,
    pub mount_id: String,
    pub connector: String,
    pub next: Vec<String>,
}

pub fn run_create_page<S>(
    store: &S,
    options: CreatePageOptions,
) -> Result<CreatePageReport, CreateError>
where
    S: MountRepository,
{
    let title = normalized_title(&options.title)?;
    let parent = match options.parent {
        Some(parent) => absolute_path(&parent)?,
        None => std::env::current_dir().map_err(|error| CreateError::CurrentDir {
            message: error.to_string(),
        })?,
    };
    let mounts = store.load_mounts().map_err(CreateError::Store)?;
    let (mount, _) = file_provider::find_mount_for_path(&mounts, &parent)
        .ok_or_else(|| CreateError::MountNotFound(parent.clone()))?;
    if mount.read_only {
        return Err(CreateError::ReadOnlyMount {
            mount_id: mount.mount_id.0.clone(),
        });
    }

    let page_directory_name = page_directory_name_for_title(&title);
    let page_dir = parent.join(&page_directory_name);
    let page_path = page_dir.join(PAGE_DOCUMENT_FILENAME);
    if page_dir.exists() {
        return Err(CreateError::TargetExists(page_dir));
    }
    fs::create_dir_all(&page_dir).map_err(|error| CreateError::WriteFile {
        path: page_dir.clone(),
        message: error.to_string(),
    })?;
    let body = format!("---\ntitle: {}\n---\n", yaml_double_quoted(&title));
    fs::write(&page_path, body).map_err(|error| {
        let _ = fs::remove_dir(&page_dir);
        CreateError::WriteFile {
            path: page_path.clone(),
            message: error.to_string(),
        }
    })?;

    let page_path_display = page_path.display().to_string();
    Ok(CreatePageReport {
        ok: true,
        command: "create_page",
        kind: "page",
        title,
        parent: parent.display().to_string(),
        directory: page_dir.display().to_string(),
        path: page_path_display.clone(),
        mount_id: mount.mount_id.0.clone(),
        connector: mount.connector.clone(),
        next: vec![
            format!("loc diff {}", shell_quote_path(&page_path_display)),
            format!("loc push {} -y", shell_quote_path(&page_path_display)),
        ],
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateError {
    CurrentDir { message: String },
    InvalidTitle(String),
    MountNotFound(PathBuf),
    ReadOnlyMount { mount_id: String },
    Store(StoreError),
    TargetExists(PathBuf),
    WriteFile { path: PathBuf, message: String },
}

impl CreateError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CurrentDir { .. } => "current_dir_failed",
            Self::InvalidTitle(_) => "invalid_title",
            Self::MountNotFound(_) => "mount_not_found",
            Self::ReadOnlyMount { .. } => "read_only_mount",
            Self::Store(_) => "store_error",
            Self::TargetExists(_) => "target_exists",
            Self::WriteFile { .. } => "write_file_failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::CurrentDir { message } => {
                format!("failed to resolve current directory: {message}")
            }
            Self::InvalidTitle(message) => message.clone(),
            Self::MountNotFound(path) => {
                format!("no Locality mount contains parent `{}`", path.display())
            }
            Self::ReadOnlyMount { mount_id } => {
                format!("mount `{mount_id}` is read-only and cannot accept new pages")
            }
            Self::Store(error) => error.to_string(),
            Self::TargetExists(path) => {
                format!("target page directory `{}` already exists", path.display())
            }
            Self::WriteFile { path, message } => {
                format!("failed to write `{}`: {message}", path.display())
            }
        }
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, CreateError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| CreateError::CurrentDir {
                message: error.to_string(),
            })
    }
}

fn normalized_title(title: &str) -> Result<String, CreateError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(CreateError::InvalidTitle(
            "page title must not be empty".to_string(),
        ));
    }
    if title == "." || title == ".." {
        return Err(CreateError::InvalidTitle(
            "page title must be a file name, not `.` or `..`".to_string(),
        ));
    }
    let path = Path::new(title);
    if path.is_absolute() || path.components().count() != 1 {
        return Err(CreateError::InvalidTitle(
            "page title must be a single path component".to_string(),
        ));
    }
    match path.components().next() {
        Some(Component::Normal(_)) => Ok(title.to_string()),
        _ => Err(CreateError::InvalidTitle(
            "page title must be a normal file name".to_string(),
        )),
    }
}

fn page_directory_name_for_title(title: &str) -> String {
    let sanitized = title
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            character if character.is_control() => '-',
            character => character,
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .to_string();

    if sanitized.is_empty() {
        "Untitled".to_string()
    } else {
        sanitized
    }
}

fn yaml_double_quoted(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    escaped
}

fn shell_quote_path(path: &str) -> String {
    if path
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        return path.to_string();
    }
    format!("'{}'", path.replace('\'', "'\\''"))
}

impl From<io::Error> for CreateError {
    fn from(error: io::Error) -> Self {
        Self::WriteFile {
            path: PathBuf::new(),
            message: error.to_string(),
        }
    }
}
