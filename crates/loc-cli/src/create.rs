//! Local draft creation helpers for `loc create`.
//!
//! Creation stays filesystem-first: this module writes the draft shape that
//! push and Live Mode already understand. It does not call remote connectors.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use locality_core::model::{EntityKind, RemoteId};
use locality_core::path_projection::{PAGE_DOCUMENT_FILENAME, page_document_path};
use locality_notion::database_create::default_database_draft_yaml;
use locality_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, StoreError, VirtualMutationKind,
    VirtualMutationRecord, VirtualMutationRepository,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateDatabaseOptions {
    pub title: String,
    pub parent: Option<PathBuf>,
    pub state_root: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateDatabaseReport {
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
    store: &mut S,
    options: CreatePageOptions,
) -> Result<CreatePageReport, CreateError>
where
    S: EntityRepository + MountRepository + VirtualMutationRepository,
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
    if mount.projection.uses_virtual_filesystem() {
        let state_root = options
            .state_root
            .as_deref()
            .ok_or(CreateError::VirtualStateRootRequired)?;
        let parent_remote_id = if options.private {
            None
        } else {
            Some(parent_remote_id_for_path(store, &mount, &parent)?)
        };
        stage_virtual_page(
            store,
            &mount,
            state_root,
            &page_dir,
            &body,
            parent_remote_id,
        )?;
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

pub fn run_create_database<S>(
    store: &mut S,
    options: CreateDatabaseOptions,
) -> Result<CreateDatabaseReport, CreateError>
where
    S: EntityRepository + MountRepository + VirtualMutationRepository,
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
    if mount.connector != "notion" {
        return Err(CreateError::DatabaseUnsupported {
            connector: mount.connector.clone(),
        });
    }
    let entities = store
        .list_entities(&mount.mount_id)
        .map_err(CreateError::Store)?;
    let parent_entity = parent_entity_for_path(&relative_path(mount, &parent)?, &entities)
        .ok_or_else(|| CreateError::InvalidParent {
            path: parent.clone(),
            message: "no existing page matches this parent directory".to_string(),
        })?;
    if parent_entity.kind != EntityKind::Page {
        return Err(CreateError::InvalidParent {
            path: parent.clone(),
            message: "Notion databases must be created inside an existing page directory"
                .to_string(),
        });
    }

    let database_dir = parent.join(page_directory_name_for_title(&title));
    let schema_path = database_dir.join("_schema.yaml");
    if database_dir.exists()
        || store
            .find_virtual_mutation_by_path(&mount.mount_id, &relative_path(mount, &schema_path)?)
            .map_err(CreateError::Store)?
            .is_some()
    {
        return Err(CreateError::TargetExists(database_dir));
    }
    let schema = default_database_draft_yaml(&title);
    if mount.projection.uses_virtual_filesystem() {
        let state_root = options
            .state_root
            .as_deref()
            .ok_or(CreateError::VirtualStateRootRequired)?;
        stage_virtual_file(
            store,
            mount,
            state_root,
            &schema_path,
            &schema,
            Some(parent_entity.remote_id.clone()),
        )?;
    } else {
        fs::create_dir_all(&database_dir).map_err(|error| CreateError::WriteFile {
            path: database_dir.clone(),
            message: error.to_string(),
        })?;
        fs::write(&schema_path, schema).map_err(|error| {
            let _ = fs::remove_dir(&database_dir);
            CreateError::WriteFile {
                path: schema_path.clone(),
                message: error.to_string(),
            }
        })?;
    }

    let path = schema_path.display().to_string();
    Ok(CreateDatabaseReport {
        ok: true,
        command: "create_database",
        kind: "database",
        title,
        parent: parent.display().to_string(),
        directory: database_dir.display().to_string(),
        path: path.clone(),
        mount_id: mount.mount_id.0.clone(),
        connector: mount.connector.clone(),
        next: vec![
            format!("loc diff {}", shell_quote_path(&path)),
            format!("loc push {} -y", shell_quote_path(&path)),
        ],
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateError {
    CurrentDir { message: String },
    InvalidTitle(String),
    MountNotFound(PathBuf),
    PrivateUnsupported { connector: String },
    DatabaseUnsupported { connector: String },
    InvalidParent { path: PathBuf, message: String },
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
            Self::DatabaseUnsupported { .. } => "database_unsupported",
            Self::InvalidParent { .. } => "invalid_parent",
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
            Self::DatabaseUnsupported { connector } => {
                format!("database creation is only supported for Notion mounts, not `{connector}`")
            }
            Self::InvalidParent { path, message } => {
                format!("cannot create inside `{}`: {message}", path.display())
            }
            Self::ReadOnlyMount { mount_id } => {
                format!("mount `{mount_id}` is read-only and cannot accept new items")
            }
            Self::VirtualStateRootRequired => {
                "creating items in virtual mounts requires a Locality state directory".to_string()
            }
            Self::Store(error) => error.to_string(),
            Self::TargetExists(path) => {
                format!("target directory `{}` already exists", path.display())
            }
            Self::WriteFile { path, message } => {
                format!("failed to write `{}`: {message}", path.display())
            }
        }
    }
}

fn stage_virtual_page<S>(
    store: &mut S,
    mount: &MountConfig,
    state_root: &Path,
    page_dir: &Path,
    body: &str,
    parent_remote_id: Option<RemoteId>,
) -> Result<(), CreateError>
where
    S: VirtualMutationRepository,
{
    let projected_path = page_document_path(&relative_path(mount, page_dir)?);
    stage_virtual_file_at_relative_path(
        store,
        mount,
        state_root,
        page_dir,
        projected_path,
        body,
        parent_remote_id,
    )
}

fn stage_virtual_file<S>(
    store: &mut S,
    mount: &MountConfig,
    state_root: &Path,
    file_path: &Path,
    body: &str,
    parent_remote_id: Option<RemoteId>,
) -> Result<(), CreateError>
where
    S: VirtualMutationRepository,
{
    let projected_path = relative_path(mount, file_path)?;
    stage_virtual_file_at_relative_path(
        store,
        mount,
        state_root,
        file_path,
        projected_path,
        body,
        parent_remote_id,
    )
}

#[allow(clippy::too_many_arguments)]
fn stage_virtual_file_at_relative_path<S>(
    store: &mut S,
    mount: &MountConfig,
    state_root: &Path,
    display_path: &Path,
    projected_path: PathBuf,
    body: &str,
    parent_remote_id: Option<RemoteId>,
) -> Result<(), CreateError>
where
    S: VirtualMutationRepository,
{
    if store
        .find_virtual_mutation_by_path(&mount.mount_id, &projected_path)
        .map_err(CreateError::Store)?
        .is_some()
    {
        return Err(CreateError::TargetExists(display_path.to_path_buf()));
    }
    let content_path = virtual_fs_content_path(state_root, &mount.mount_id, &projected_path)
        .map_err(|error| CreateError::WriteFile {
            path: display_path.to_path_buf(),
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
    let title = if is_database_schema_path(&projected_path) {
        projected_path.parent().and_then(Path::file_name)
    } else {
        display_path.file_name()
    }
    .map(|name| name.to_string_lossy().into_owned())
    .unwrap_or_else(|| "Untitled".to_string());
    store
        .save_virtual_mutation(VirtualMutationRecord {
            mount_id: mount.mount_id.clone(),
            local_id: local_create_id(),
            mutation_kind: VirtualMutationKind::Create,
            target_remote_id: None,
            parent_remote_id,
            original_path: None,
            projected_path,
            title,
            content_path: Some(content_path),
            created_at: now.clone(),
            updated_at: now,
        })
        .map_err(CreateError::Store)
}

fn is_database_schema_path(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("_schema.yaml")
}

fn parent_remote_id_for_path<S>(
    store: &S,
    mount: &MountConfig,
    parent: &Path,
) -> Result<RemoteId, CreateError>
where
    S: EntityRepository,
{
    let relative_parent = relative_path(mount, parent)?;
    if relative_parent.as_os_str().is_empty() {
        return Err(CreateError::InvalidParent {
            path: parent.to_path_buf(),
            message: "new pages must be created inside an existing page or database directory"
                .to_string(),
        });
    }

    let entities = store
        .list_entities(&mount.mount_id)
        .map_err(CreateError::Store)?;
    parent_entity_for_path(&relative_parent, &entities)
        .map(|entity| entity.remote_id.clone())
        .ok_or_else(|| CreateError::InvalidParent {
            path: parent.to_path_buf(),
            message: "no existing page or database matches this parent directory".to_string(),
        })
}

fn parent_entity_for_path<'a>(
    relative_parent: &Path,
    entities: &'a [EntityRecord],
) -> Option<&'a EntityRecord> {
    let parent_page_path = page_document_path(relative_parent);
    entities.iter().find(|entity| match entity.kind {
        EntityKind::Page => entity.path == parent_page_path,
        EntityKind::Database => entity.path == relative_parent,
        EntityKind::Directory | EntityKind::Asset | EntityKind::Unknown(_) => false,
    })
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
