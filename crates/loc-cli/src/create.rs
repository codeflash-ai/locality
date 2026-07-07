//! Local draft creation helpers for `loc create`.
//!
//! Creation stays filesystem-first: this module writes the draft shape that
//! push and Live Mode already understand. It does not call remote connectors.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::path_projection::{PAGE_DOCUMENT_FILENAME, page_document_path};
use locality_store::{
    MountConfig, MountRepository, StoreError, VirtualMutationKind, VirtualMutationRecord,
    VirtualMutationRepository,
};
use localityd::file_provider;
use localityd::virtual_fs::virtual_fs_content_path;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatePageOptions {
    pub title: String,
    pub parent: Option<PathBuf>,
    pub private: bool,
    pub state_root: Option<PathBuf>,
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
    pub private: bool,
    pub next: Vec<String>,
}

pub fn run_create_page<S>(
    store: &mut S,
    options: CreatePageOptions,
) -> Result<CreatePageReport, CreateError>
where
    S: MountRepository + VirtualMutationRepository,
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
    if options.private && mount.connector != "notion" {
        return Err(CreateError::PrivateUnsupported {
            connector: mount.connector.clone(),
        });
    }

    let page_directory_name = page_directory_name_for_title(&title);
    let page_dir = parent.join(&page_directory_name);
    let page_path = page_dir.join(PAGE_DOCUMENT_FILENAME);
    if page_dir.exists() {
        return Err(CreateError::TargetExists(page_dir));
    }
    let body = if options.private {
        format!(
            "---\nloc:\n  private: true\ntitle: {}\n---\n",
            yaml_double_quoted(&title)
        )
    } else {
        format!("---\ntitle: {}\n---\n", yaml_double_quoted(&title))
    };
    if options.private && mount.projection.uses_virtual_filesystem() {
        let state_root = options
            .state_root
            .as_deref()
            .ok_or(CreateError::VirtualStateRootRequired)?;
        stage_private_virtual_page(store, &mount, state_root, &page_dir, &body)?;
    } else {
        fs::create_dir_all(&page_dir).map_err(|error| CreateError::WriteFile {
            path: page_dir.clone(),
            message: error.to_string(),
        })?;
        fs::write(&page_path, body).map_err(|error| {
            let _ = fs::remove_dir(&page_dir);
            CreateError::WriteFile {
                path: page_path.clone(),
                message: error.to_string(),
            }
        })?;
    }

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
        private: options.private,
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
    PrivateUnsupported { connector: String },
    ReadOnlyMount { mount_id: String },
    VirtualStateRootRequired,
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
            Self::PrivateUnsupported { .. } => "private_unsupported",
            Self::ReadOnlyMount { .. } => "read_only_mount",
            Self::VirtualStateRootRequired => "virtual_state_root_required",
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
            Self::PrivateUnsupported { connector } => {
                format!("--private is only supported for Notion mounts, not `{connector}`")
            }
            Self::ReadOnlyMount { mount_id } => {
                format!("mount `{mount_id}` is read-only and cannot accept new pages")
            }
            Self::VirtualStateRootRequired => {
                "creating private pages in virtual mounts requires a Locality state directory"
                    .to_string()
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

fn stage_private_virtual_page<S>(
    store: &mut S,
    mount: &MountConfig,
    state_root: &Path,
    page_dir: &Path,
    body: &str,
) -> Result<(), CreateError>
where
    S: VirtualMutationRepository,
{
    let projected_path = page_document_path(&relative_path(mount, page_dir)?);
    if store
        .find_virtual_mutation_by_path(&mount.mount_id, &projected_path)
        .map_err(CreateError::Store)?
        .is_some()
    {
        return Err(CreateError::TargetExists(page_dir.to_path_buf()));
    }
    let content_path = virtual_fs_content_path(state_root, &mount.mount_id, &projected_path)
        .map_err(|error| CreateError::WriteFile {
            path: page_dir.to_path_buf(),
            message: error.to_string(),
        })?;
    if let Some(parent) = content_path.parent() {
        fs::create_dir_all(parent).map_err(|error| CreateError::WriteFile {
            path: parent.to_path_buf(),
            message: error.to_string(),
        })?;
    }
    fs::write(&content_path, body).map_err(|error| CreateError::WriteFile {
        path: content_path.clone(),
        message: error.to_string(),
    })?;
    let now = timestamp_string();
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: mount.mount_id.clone(),
            local_id: local_create_id(),
            mutation_kind: VirtualMutationKind::Create,
            target_remote_id: None,
            parent_remote_id: None,
            original_path: None,
            projected_path,
            title: page_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Untitled".to_string()),
            content_path: Some(content_path),
            created_at: now.clone(),
            updated_at: now,
        })
        .map_err(CreateError::Store)
}

fn relative_path(mount: &MountConfig, path: &Path) -> Result<PathBuf, CreateError> {
    path.strip_prefix(&mount.root)
        .map(Path::to_path_buf)
        .map_err(|error| CreateError::WriteFile {
            path: path.to_path_buf(),
            message: error.to_string(),
        })
}

fn local_create_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "local:create-page-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn timestamp_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| format!("unix_ms:{}", duration.as_millis()))
        .unwrap_or_else(|_| "unix_ms:0".to_string())
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
