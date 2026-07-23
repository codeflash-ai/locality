//! Local draft creation helpers for `loc create`.
//!
//! Creation stays filesystem-first: this module writes the draft shape that
//! push and Live Mode already understand. It does not call remote connectors.

use std::fs::{self, OpenOptions};
use std::io;
use std::io::Write as _;
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
use localityd::source::{source_create_decision_for_parent_path, source_display_name};
use localityd::virtual_fs::virtual_fs_content_path;
use serde::{Deserialize, Serialize};

const MAX_GMAIL_REPLY_FILENAME_BYTES: usize = 128;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateGmailReplyOptions {
    pub thread: PathBuf,
    pub message: Option<String>,
    pub state_root: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateGmailReplyReport {
    pub ok: bool,
    pub command: &'static str,
    pub kind: &'static str,
    pub path: String,
    pub mount_id: String,
    pub thread_id: String,
    pub reply_to_message_id: String,
    pub recipient: String,
    pub subject: String,
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
    let relative_parent = relative_path(mount, &parent)?;
    let create_decision = source_create_decision_for_parent_path(mount, &relative_parent);
    if let Some(reason) = create_decision.reason() {
        return Err(CreateError::ReadOnlySource {
            mount_id: mount.mount_id.0.clone(),
            connector: mount.connector.clone(),
            reason: reason.to_string(),
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

pub fn run_create_gmail_reply<S>(
    store: &mut S,
    options: CreateGmailReplyOptions,
) -> Result<CreateGmailReplyReport, CreateError>
where
    S: EntityRepository + MountRepository + VirtualMutationRepository,
{
    let mut thread = absolute_path(&options.thread)?;
    if thread.file_name().and_then(|name| name.to_str()) == Some(PAGE_DOCUMENT_FILENAME) {
        thread = thread.parent().map(Path::to_path_buf).ok_or_else(|| {
            CreateError::InvalidReply("thread page has no parent directory".into())
        })?;
    }
    let mounts = store.load_mounts().map_err(CreateError::Store)?;
    let (mount, _) = file_provider::find_mount_for_path(&mounts, &thread)
        .ok_or_else(|| CreateError::MountNotFound(thread.clone()))?;
    if mount.read_only {
        return Err(CreateError::ReadOnlyMount {
            mount_id: mount.mount_id.0.clone(),
        });
    }
    if mount.connector != "gmail" {
        return Err(CreateError::GmailReplyUnsupported {
            connector: mount.connector.clone(),
        });
    }
    let relative_thread = relative_path(mount, &thread)?;
    if !matches!(relative_thread.components().next(), Some(Component::Normal(folder)) if folder == "inbox" || folder == "sent")
    {
        return Err(CreateError::InvalidReply(
            "Gmail replies must target an inbox/ or sent/ thread directory".to_string(),
        ));
    }

    let mut messages = Vec::new();
    for entry in fs::read_dir(&thread).map_err(|error| CreateError::WriteFile {
        path: thread.clone(),
        message: error.to_string(),
    })? {
        let entry = entry.map_err(|error| CreateError::WriteFile {
            path: thread.clone(),
            message: error.to_string(),
        })?;
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(PAGE_DOCUMENT_FILENAME)
            || path.extension().and_then(|extension| extension.to_str()) != Some("md")
        {
            continue;
        }
        let content = fs::read_to_string(&path).map_err(|error| CreateError::WriteFile {
            path: path.clone(),
            message: error.to_string(),
        })?;
        if let Some(frontmatter) = markdown_frontmatter(&content) {
            let parsed =
                yaml_serde::from_str::<ReplyMessageFrontmatter>(frontmatter).map_err(|error| {
                    CreateError::InvalidReply(format!(
                        "cannot read Gmail metadata from `{}`: {error}",
                        path.display()
                    ))
                })?;
            if parsed.gmail.thread_id.trim().is_empty()
                || parsed.gmail.message_id.trim().is_empty()
                || parsed.gmail.rfc_message_id.trim().is_empty()
            {
                continue;
            }
            messages.push(ReplyMessage { path, parsed });
        }
    }
    if messages.is_empty() {
        return Err(CreateError::InvalidReply(
            "thread has no hydrated message files with Gmail reply metadata; open or pull a message first"
                .to_string(),
        ));
    }

    let selected = if let Some(selector) = options.message.as_deref() {
        let selector_path = Path::new(selector);
        messages
            .iter()
            .find(|message| {
                message.path == selector_path
                    || message.path.file_name() == selector_path.file_name()
                    || message.path.strip_prefix(&thread).ok() == Some(selector_path)
                    || message.parsed.gmail.message_id == selector
            })
            .ok_or_else(|| {
                CreateError::InvalidReply(format!(
                    "no hydrated message in the thread matches `{selector}`"
                ))
            })?
    } else {
        messages
            .iter()
            .max_by(|left, right| reply_message_order(left).cmp(&reply_message_order(right)))
            .expect("messages is not empty")
    };

    let thread_id = selected.parsed.gmail.thread_id.trim().to_string();
    if messages
        .iter()
        .any(|message| message.parsed.gmail.thread_id.trim() != thread_id)
    {
        return Err(CreateError::InvalidReply(
            "thread directory contains messages from different Gmail threads".to_string(),
        ));
    }
    let recipient = selected
        .parsed
        .gmail
        .reply_to
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&selected.parsed.from)
        .trim()
        .to_string();
    if recipient.is_empty() {
        return Err(CreateError::InvalidReply(
            "selected message has neither Reply-To nor From metadata".to_string(),
        ));
    }
    let subject = selected.parsed.subject.trim().to_string();
    if subject.is_empty() {
        return Err(CreateError::InvalidReply(
            "selected message has no subject".to_string(),
        ));
    }
    let rfc_message_id = selected.parsed.gmail.rfc_message_id.trim().to_string();
    let mut references = selected.parsed.gmail.references.clone();
    if !references
        .iter()
        .any(|reference| reference == &rfc_message_id)
    {
        references.push(rfc_message_id.clone());
    }
    let body = gmail_reply_markdown(
        &recipient,
        &subject,
        &thread_id,
        &selected.parsed.gmail.message_id,
        &rfc_message_id,
        &references,
    );
    let draft_dir = mount.root.join("draft");
    let draft_path = unique_reply_draft_path(&draft_dir, &subject);
    if mount.projection.uses_virtual_filesystem() {
        let state_root = options
            .state_root
            .as_deref()
            .ok_or(CreateError::VirtualStateRootRequired)?;
        stage_virtual_file(
            store,
            mount,
            state_root,
            &draft_path,
            &body,
            Some(RemoteId::new("gmail-folder:draft")),
        )?;
    } else {
        fs::create_dir_all(&draft_dir).map_err(|error| CreateError::WriteFile {
            path: draft_dir.clone(),
            message: error.to_string(),
        })?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&draft_path)
            .map_err(|error| CreateError::WriteFile {
                path: draft_path.clone(),
                message: error.to_string(),
            })?;
        file.write_all(body.as_bytes())
            .map_err(|error| CreateError::WriteFile {
                path: draft_path.clone(),
                message: error.to_string(),
            })?;
    }

    let path = draft_path.display().to_string();
    Ok(CreateGmailReplyReport {
        ok: true,
        command: "create_gmail_reply",
        kind: "gmail_reply_draft",
        path: path.clone(),
        mount_id: mount.mount_id.0.clone(),
        thread_id,
        reply_to_message_id: selected.parsed.gmail.message_id.clone(),
        recipient,
        subject,
        next: vec![
            format!("loc diff {}", shell_quote_path(&path)),
            format!("loc push {} -y", shell_quote_path(&path)),
        ],
    })
}

#[derive(Debug, Deserialize)]
struct ReplyMessageFrontmatter {
    #[serde(default)]
    from: String,
    #[serde(default)]
    subject: String,
    gmail: ReplyGmailFrontmatter,
}

#[derive(Debug, Deserialize)]
struct ReplyGmailFrontmatter {
    message_id: String,
    thread_id: String,
    rfc_message_id: String,
    reply_to: Option<String>,
    #[serde(default)]
    references: Vec<String>,
    internal_date: Option<String>,
}

struct ReplyMessage {
    path: PathBuf,
    parsed: ReplyMessageFrontmatter,
}

fn markdown_frontmatter(markdown: &str) -> Option<&str> {
    let rest = markdown.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

fn reply_message_order(message: &ReplyMessage) -> (u64, String) {
    let internal_date = message
        .parsed
        .gmail
        .internal_date
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    (internal_date, message.path.display().to_string())
}

fn unique_reply_draft_path(draft_dir: &Path, subject: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let prefix = format!("reply-{stamp}-{counter}-");
    let suffix = ".md";
    let slug_budget = MAX_GMAIL_REPLY_FILENAME_BYTES
        .saturating_sub(prefix.len())
        .saturating_sub(suffix.len());
    let mut slug = page_directory_name_for_title(subject);
    if slug.len() > slug_budget {
        let mut end = slug_budget;
        while !slug.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        slug.truncate(end);
    }
    draft_dir.join(format!("{prefix}{slug}{suffix}"))
}

fn gmail_reply_markdown(
    recipient: &str,
    subject: &str,
    thread_id: &str,
    reply_to_message_id: &str,
    in_reply_to: &str,
    references: &[String],
) -> String {
    let mut output = format!(
        "---\nto: {}\nsubject: {}\ngmail:\n  thread_id: {}\n  reply_to_message_id: {}\n  in_reply_to: {}\n  references:\n",
        yaml_double_quoted(recipient),
        yaml_double_quoted(subject),
        yaml_double_quoted(thread_id),
        yaml_double_quoted(reply_to_message_id),
        yaml_double_quoted(in_reply_to),
    );
    for reference in references {
        output.push_str(&format!("    - {}\n", yaml_double_quoted(reference)));
    }
    output.push_str("---\n\n");
    output
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateError {
    CurrentDir {
        message: String,
    },
    InvalidTitle(String),
    MountNotFound(PathBuf),
    PrivateUnsupported {
        connector: String,
    },
    DatabaseUnsupported {
        connector: String,
    },
    GmailReplyUnsupported {
        connector: String,
    },
    InvalidReply(String),
    InvalidParent {
        path: PathBuf,
        message: String,
    },
    ReadOnlyMount {
        mount_id: String,
    },
    ReadOnlySource {
        mount_id: String,
        connector: String,
        reason: String,
    },
    VirtualStateRootRequired,
    Store(StoreError),
    TargetExists(PathBuf),
    WriteFile {
        path: PathBuf,
        message: String,
    },
}

impl CreateError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CurrentDir { .. } => "current_dir_failed",
            Self::InvalidTitle(_) => "invalid_title",
            Self::MountNotFound(_) => "mount_not_found",
            Self::PrivateUnsupported { .. } => "private_unsupported",
            Self::DatabaseUnsupported { .. } => "database_unsupported",
            Self::GmailReplyUnsupported { .. } => "gmail_reply_unsupported",
            Self::InvalidReply(_) => "invalid_gmail_reply",
            Self::InvalidParent { .. } => "invalid_parent",
            Self::ReadOnlyMount { .. } => "read_only_mount",
            Self::ReadOnlySource { .. } => "read_only_source",
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
            Self::GmailReplyUnsupported { connector } => {
                format!("Gmail replies are only supported for Gmail mounts, not `{connector}`")
            }
            Self::InvalidReply(message) => message.clone(),
            Self::InvalidParent { path, message } => {
                format!("cannot create inside `{}`: {message}", path.display())
            }
            Self::ReadOnlyMount { mount_id } => {
                format!("mount `{mount_id}` is read-only and cannot accept new items")
            }
            Self::ReadOnlySource {
                mount_id,
                connector,
                reason,
            } => {
                let source = source_display_name(connector);
                format!("{source} mount `{mount_id}` cannot accept new pages: {reason}")
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
