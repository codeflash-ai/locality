//! `loc mount` orchestration.
//!
//! This first real mount command records enough connector configuration for the
//! pull path to build a filesystem projection from a Notion root page and drops
//! concise agent guidance into the mount root.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use locality_core::model::{MountId, RemoteId};
use locality_store::{ConnectionId, MountConfig, MountRepository, ProjectionMode, StoreError};
use localityd::source::source_descriptor;
use serde::Serialize;

const AGENTS_FILE: &str = "AGENTS.md";
const CLAUDE_FILE: &str = "CLAUDE.md";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountOptions {
    pub mount_id: MountId,
    pub connector: String,
    pub root: PathBuf,
    pub remote_root_id: Option<RemoteId>,
    pub connection_id: Option<ConnectionId>,
    pub read_only: bool,
    pub projection: ProjectionMode,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MountReport {
    pub ok: bool,
    pub command: &'static str,
    pub mount_id: String,
    pub connector: String,
    pub root: String,
    pub remote_root_id: Option<String>,
    pub connection_id: Option<String>,
    pub read_only: bool,
    pub projection: String,
    pub guidance: MountGuidanceReport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MountGuidanceReport {
    pub agents_md: GuidanceFileReport,
    pub claude_md: GuidanceFileReport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GuidanceFileReport {
    pub path: String,
    pub action: GuidanceFileAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuidanceFileAction {
    Created,
    Preserved,
    Symlinked,
    Copied,
    Virtual,
}

impl GuidanceFileAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Preserved => "preserved",
            Self::Symlinked => "symlinked",
            Self::Copied => "copied",
            Self::Virtual => "virtual",
        }
    }
}

pub fn run_mount<S>(store: &mut S, options: MountOptions) -> Result<MountReport, MountError>
where
    S: MountRepository,
{
    let root = absolute_path(&options.root)?;
    if options.projection.uses_virtual_filesystem() {
        reject_duplicate_virtual_mount_point(store, &options.mount_id, &root, &options.projection)?;
    }

    let guidance = if options.projection == ProjectionMode::MacosFileProvider {
        virtual_mount_guidance(&root)
    } else {
        std::fs::create_dir_all(&root).map_err(|error| MountError::CreateRoot {
            path: root.clone(),
            message: error.to_string(),
        })?;
        install_mount_guidance(&root, &options.connector)?
    };

    let mut mount = MountConfig::new(options.mount_id.clone(), options.connector.clone(), &root)
        .read_only(options.read_only)
        .projection(options.projection.clone());
    if let Some(remote_root_id) = options.remote_root_id.clone() {
        mount = mount.with_remote_root_id(remote_root_id);
    }
    if let Some(connection_id) = options.connection_id.clone() {
        mount = mount.with_connection_id(connection_id);
    }

    store.save_mount(mount).map_err(MountError::Store)?;

    Ok(MountReport {
        ok: true,
        command: "mount",
        mount_id: options.mount_id.0,
        connector: options.connector,
        root: root.display().to_string(),
        remote_root_id: options.remote_root_id.map(|remote_id| remote_id.0),
        connection_id: options.connection_id.map(|connection_id| connection_id.0),
        read_only: options.read_only,
        projection: options.projection.as_str().to_string(),
        guidance,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountError {
    CreateRoot {
        path: PathBuf,
        message: String,
    },
    CurrentDir(String),
    MountPointConflict {
        root: PathBuf,
        mount_point: String,
        existing_mount_id: MountId,
    },
    ReadGuidance {
        path: PathBuf,
        message: String,
    },
    Store(StoreError),
    WriteGuidance {
        path: PathBuf,
        message: String,
    },
}

impl MountError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CreateRoot { .. } => "create_mount_root_failed",
            Self::CurrentDir(_) => "current_dir_failed",
            Self::MountPointConflict { .. } => "mount_point_conflict",
            Self::ReadGuidance { .. } => "read_mount_guidance_failed",
            Self::Store(_) => "store_error",
            Self::WriteGuidance { .. } => "write_mount_guidance_failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::CreateRoot { path, message } => {
                format!(
                    "failed to create mount root `{}`: {message}",
                    path.display()
                )
            }
            Self::CurrentDir(message) => format!("failed to resolve current directory: {message}"),
            Self::MountPointConflict {
                root,
                mount_point,
                existing_mount_id,
            } => format!(
                "mount `{}` already uses mount point `{mount_point}` under `{}`",
                existing_mount_id.0,
                root.display()
            ),
            Self::ReadGuidance { path, message } => {
                format!(
                    "failed to read mount guidance `{}`: {message}",
                    path.display()
                )
            }
            Self::Store(error) => error.to_string(),
            Self::WriteGuidance { path, message } => {
                format!(
                    "failed to write mount guidance `{}`: {message}",
                    path.display()
                )
            }
        }
    }
}

fn reject_duplicate_virtual_mount_point<S>(
    store: &S,
    mount_id: &MountId,
    root: &Path,
    projection: &ProjectionMode,
) -> Result<(), MountError>
where
    S: MountRepository,
{
    let proposed =
        MountConfig::new(mount_id.clone(), "pending", root).projection(projection.clone());
    let proposed_root = localityd::virtual_fs::virtual_projection_root(&proposed);
    let proposed_mount_point = localityd::virtual_fs::mount_point_directory_name(&proposed);

    for existing in store.load_mounts().map_err(MountError::Store)? {
        if existing.mount_id == *mount_id || existing.projection != *projection {
            continue;
        }
        if !existing.projection.uses_virtual_filesystem() {
            continue;
        }
        if localityd::virtual_fs::virtual_projection_root(&existing) == proposed_root
            && localityd::virtual_fs::mount_point_directory_name(&existing) == proposed_mount_point
        {
            return Err(MountError::MountPointConflict {
                root: proposed_root,
                mount_point: proposed_mount_point,
                existing_mount_id: existing.mount_id,
            });
        }
    }

    Ok(())
}

fn install_mount_guidance(root: &Path, connector: &str) -> Result<MountGuidanceReport, MountError> {
    let agents_path = root.join(AGENTS_FILE);
    let claude_path = root.join(CLAUDE_FILE);
    let descriptor = source_descriptor(connector);
    let agents_action = write_guidance_if_absent(&agents_path, descriptor.mount_guidance())?;
    let claude_action = install_claude_guidance(&agents_path, &claude_path)?;

    Ok(MountGuidanceReport {
        agents_md: GuidanceFileReport {
            path: agents_path.display().to_string(),
            action: agents_action,
        },
        claude_md: GuidanceFileReport {
            path: claude_path.display().to_string(),
            action: claude_action,
        },
    })
}

fn virtual_mount_guidance(root: &Path) -> MountGuidanceReport {
    MountGuidanceReport {
        agents_md: GuidanceFileReport {
            path: root.join(AGENTS_FILE).display().to_string(),
            action: GuidanceFileAction::Virtual,
        },
        claude_md: GuidanceFileReport {
            path: root.join(CLAUDE_FILE).display().to_string(),
            action: GuidanceFileAction::Virtual,
        },
    }
}

fn write_guidance_if_absent(path: &Path, contents: &str) -> Result<GuidanceFileAction, MountError> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(contents.as_bytes())
                .map_err(|error| MountError::WriteGuidance {
                    path: path.to_path_buf(),
                    message: error.to_string(),
                })?;
            Ok(GuidanceFileAction::Created)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Ok(GuidanceFileAction::Preserved)
        }
        Err(error) => Err(MountError::WriteGuidance {
            path: path.to_path_buf(),
            message: error.to_string(),
        }),
    }
}

fn install_claude_guidance(
    agents_path: &Path,
    claude_path: &Path,
) -> Result<GuidanceFileAction, MountError> {
    if claude_path
        .try_exists()
        .map_err(|error| MountError::WriteGuidance {
            path: claude_path.to_path_buf(),
            message: error.to_string(),
        })?
    {
        return Ok(GuidanceFileAction::Preserved);
    }

    symlink_agents_guidance(claude_path).or_else(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            Ok(GuidanceFileAction::Preserved)
        } else {
            copy_agents_guidance(agents_path, claude_path)
        }
    })
}

#[cfg(unix)]
fn symlink_agents_guidance(claude_path: &Path) -> io::Result<GuidanceFileAction> {
    std::os::unix::fs::symlink(AGENTS_FILE, claude_path)?;
    Ok(GuidanceFileAction::Symlinked)
}

#[cfg(not(unix))]
fn symlink_agents_guidance(_claude_path: &Path) -> io::Result<GuidanceFileAction> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic links are not used on this platform",
    ))
}

fn copy_agents_guidance(
    agents_path: &Path,
    claude_path: &Path,
) -> Result<GuidanceFileAction, MountError> {
    let contents = fs::read_to_string(agents_path).map_err(|error| MountError::ReadGuidance {
        path: agents_path.to_path_buf(),
        message: error.to_string(),
    })?;

    match write_guidance_if_absent(claude_path, &contents)? {
        GuidanceFileAction::Created => Ok(GuidanceFileAction::Copied),
        action => Ok(action),
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, MountError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| MountError::CurrentDir(error.to_string()))
    }
}
