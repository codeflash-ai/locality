//! Local-first move helpers for `loc mv`.
//!
//! The command stages a local move/rename only. Plain-files mounts use a guarded
//! filesystem rename. Virtual mounts route through the same virtual filesystem
//! rename path used by File Provider and FUSE.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use locality_core::LocalityError;
use locality_core::model::EntityKind;
use locality_core::path_projection::{
    is_page_document_path, page_container_path, page_document_path,
};
use locality_store::{
    EntityRepository, FreshnessStateRepository, MountConfig, MountRepository, ShadowRepository,
    SqliteStateStore, StoreError, VirtualMoveRepository, VirtualMutationKind,
    VirtualMutationRepository,
};
use localityd::file_provider;
use localityd::ipc::{DaemonClientError, DaemonRequest, send_request_with_timeout};
use localityd::virtual_fs::{
    VirtualFsMutationReport, mount_point_identifier, rename_virtual_fs_item,
    virtual_fs_content_root,
};
use serde::Serialize;

const DEFAULT_DAEMON_MUTATING_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MvOptions {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub state_root: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MvReport {
    pub ok: bool,
    pub command: &'static str,
    pub action: &'static str,
    pub source: String,
    pub destination: String,
    pub mount_id: String,
    pub connector: String,
    pub projection: String,
    pub mode: String,
    pub pushed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_identifier: Option<String>,
    pub next: Vec<String>,
    pub message: String,
}

pub fn run_mv<S>(store: &mut S, options: MvOptions) -> Result<MvReport, MvError>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + VirtualMoveRepository
        + FreshnessStateRepository,
{
    let prepared = prepare_move(store, &options)?;
    execute_prepared_move_direct(store, &prepared, options.state_root.as_deref())
}

pub fn run_mv_with_daemon_at_state_root(
    store: &mut SqliteStateStore,
    options: MvOptions,
) -> Result<MvReport, MvError> {
    let prepared = prepare_move(store, &options)?;
    match &prepared.kind {
        PreparedMoveKind::PlainFiles => {
            execute_prepared_move_direct(store, &prepared, options.state_root.as_deref())
        }
        PreparedMoveKind::VirtualFs(request) => {
            let state_root = options
                .state_root
                .as_deref()
                .ok_or(MvError::VirtualStateRootRequired)?;
            execute_virtual_move_with_daemon_or_direct(store, state_root, &prepared, request)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MvError {
    CurrentDir {
        message: String,
    },
    MountNotFound {
        role: &'static str,
        path: PathBuf,
    },
    CrossMount {
        source_mount_id: String,
        destination_mount_id: String,
    },
    MountRootMove {
        path: PathBuf,
    },
    MissingSource(PathBuf),
    MissingDestinationParent(PathBuf),
    InvalidFilename {
        path: PathBuf,
        message: String,
    },
    ReadOnlyMount {
        mount_id: String,
    },
    DestinationExists(PathBuf),
    UnsupportedVirtualTarget {
        path: PathBuf,
        message: String,
    },
    VirtualStateRootRequired,
    Store(StoreError),
    Io {
        path: PathBuf,
        message: String,
    },
    DaemonError {
        code: String,
        message: String,
    },
    DaemonTimeout {
        timeout_ms: u128,
    },
    Locality(LocalityError),
}

impl MvError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CurrentDir { .. } => "current_dir_failed",
            Self::MountNotFound { .. } => "mount_not_found",
            Self::CrossMount { .. } => "cross_mount_move",
            Self::MountRootMove { .. } => "mount_root_move",
            Self::MissingSource(_) => "source_missing",
            Self::MissingDestinationParent(_) => "destination_parent_missing",
            Self::InvalidFilename { .. } => "invalid_filename",
            Self::ReadOnlyMount { .. } => "read_only_mount",
            Self::DestinationExists(_) => "destination_exists",
            Self::UnsupportedVirtualTarget { .. } => "unsupported_virtual_target",
            Self::VirtualStateRootRequired => "virtual_state_root_required",
            Self::Store(_) => "store_error",
            Self::Io { .. } => "io_error",
            Self::DaemonError { .. } => "daemon_error",
            Self::DaemonTimeout { .. } => "daemon_timeout",
            Self::Locality(LocalityError::NotImplemented(_)) => "not_implemented",
            Self::Locality(LocalityError::Unsupported(_)) => "unsupported",
            Self::Locality(LocalityError::Validation(_)) => "validation_failed",
            Self::Locality(LocalityError::Conflict(_)) => "conflict",
            Self::Locality(LocalityError::Guardrail(_)) => "guardrail",
            Self::Locality(LocalityError::RemoteNotFound(_)) => "remote_not_found",
            Self::Locality(LocalityError::RateLimited { .. }) => "rate_limited",
            Self::Locality(LocalityError::InvalidState(_)) => "invalid_state",
            Self::Locality(LocalityError::UpdateRequired { .. }) => "update_required",
            Self::Locality(LocalityError::Io(_)) => "io_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::CurrentDir { message } => {
                format!("failed to resolve current directory: {message}")
            }
            Self::MountNotFound { role, path } => {
                format!("no Locality mount contains {role} `{}`", path.display())
            }
            Self::CrossMount {
                source_mount_id,
                destination_mount_id,
            } => format!(
                "source is in mount `{source_mount_id}` but destination is in mount `{destination_mount_id}`"
            ),
            Self::MountRootMove { path } => {
                format!("cannot move Locality mount root `{}`", path.display())
            }
            Self::MissingSource(path) => format!("source `{}` does not exist", path.display()),
            Self::MissingDestinationParent(path) => {
                format!("destination parent `{}` does not exist", path.display())
            }
            Self::InvalidFilename { path, message } => {
                format!(
                    "invalid destination filename `{}`: {message}",
                    path.display()
                )
            }
            Self::ReadOnlyMount { mount_id } => {
                format!("mount `{mount_id}` is read-only and cannot accept moves")
            }
            Self::DestinationExists(path) => {
                format!("destination `{}` already exists", path.display())
            }
            Self::UnsupportedVirtualTarget { path, message } => {
                format!(
                    "cannot move virtual filesystem item `{}`: {message}",
                    path.display()
                )
            }
            Self::VirtualStateRootRequired => {
                "moving items in virtual mounts requires a Locality state directory".to_string()
            }
            Self::Store(error) => error.to_string(),
            Self::Io { path, message } => {
                format!("failed to move `{}`: {message}", path.display())
            }
            Self::DaemonError { code, message } => format!("{code}: {message}"),
            Self::DaemonTimeout { timeout_ms } => format!(
                "localityd did not respond within {timeout_ms}ms after the virtual move request was submitted"
            ),
            Self::Locality(error) => error.to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PreparedMove {
    mount: MountConfig,
    source_absolute: PathBuf,
    destination_absolute: PathBuf,
    kind: PreparedMoveKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PreparedMoveKind {
    PlainFiles,
    VirtualFs(VirtualMoveRequest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VirtualMoveRequest {
    identifier: String,
    new_parent_identifier: String,
    new_filename: String,
    source_kind: VirtualSourceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VirtualSourceKind {
    File,
    PageDirectory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VirtualSource {
    identifier: String,
    kind: VirtualSourceKind,
}

fn prepare_move<S>(store: &S, options: &MvOptions) -> Result<PreparedMove, MvError>
where
    S: MountRepository + EntityRepository + VirtualMutationRepository,
{
    let source_absolute = absolute_path(&options.source)?;
    let requested_destination = absolute_path(&options.destination)?;
    let mounts = store.load_mounts().map_err(MvError::Store)?;
    let (source_mount, source_match) =
        file_provider::find_mount_for_path(&mounts, &source_absolute).ok_or_else(|| {
            MvError::MountNotFound {
                role: "source",
                path: source_absolute.clone(),
            }
        })?;
    if source_match.relative_path.as_os_str().is_empty() {
        return Err(MvError::MountRootMove {
            path: source_absolute,
        });
    }
    if source_mount.read_only {
        return Err(MvError::ReadOnlyMount {
            mount_id: source_mount.mount_id.0.clone(),
        });
    }

    let (destination_mount, destination_match) =
        file_provider::find_mount_for_path(&mounts, &requested_destination).ok_or_else(|| {
            MvError::MountNotFound {
                role: "destination",
                path: requested_destination.clone(),
            }
        })?;
    if source_mount.mount_id != destination_mount.mount_id {
        return Err(MvError::CrossMount {
            source_mount_id: source_mount.mount_id.0.clone(),
            destination_mount_id: destination_mount.mount_id.0.clone(),
        });
    }

    let source_filename = source_match
        .relative_path
        .file_name()
        .ok_or_else(|| MvError::InvalidFilename {
            path: source_absolute.clone(),
            message: "source must have a final path component".to_string(),
        })?
        .to_os_string();
    let source = if source_mount.projection.uses_virtual_filesystem() {
        Some(virtual_source_for_path(
            store,
            source_mount,
            &source_match.relative_path,
            &source_absolute,
        )?)
    } else {
        if !path_exists_or_symlink(&source_absolute) {
            return Err(MvError::MissingSource(source_absolute));
        }
        None
    };

    let destination_is_directory = if source_mount.projection.uses_virtual_filesystem() {
        path_is_directory(&requested_destination)
            || virtual_container_identifier_for_path(
                store,
                source_mount,
                &destination_match.relative_path,
            )?
            .is_some()
    } else {
        path_is_directory(&requested_destination)
    };
    let destination_absolute = if destination_is_directory {
        requested_destination.join(&source_filename)
    } else {
        requested_destination
    };
    let (final_destination_mount, final_destination_match) =
        file_provider::find_mount_for_path(&mounts, &destination_absolute).ok_or_else(|| {
            MvError::MountNotFound {
                role: "destination",
                path: destination_absolute.clone(),
            }
        })?;
    if source_mount.mount_id != final_destination_mount.mount_id {
        return Err(MvError::CrossMount {
            source_mount_id: source_mount.mount_id.0.clone(),
            destination_mount_id: final_destination_mount.mount_id.0.clone(),
        });
    }
    validate_filename(&destination_absolute)?;

    if source_mount.projection.uses_virtual_filesystem() {
        let source = source.expect("virtual source resolved");
        let parent_relative = parent_relative_path(&final_destination_match.relative_path);
        let parent_identifier =
            virtual_container_identifier_for_path(store, source_mount, &parent_relative)?
                .ok_or_else(|| {
                    MvError::MissingDestinationParent(source_mount.root.join(&parent_relative))
                })?;
        let new_filename = destination_filename_string(&destination_absolute)?;
        validate_virtual_destination(
            store,
            source_mount,
            &final_destination_match.relative_path,
            source.kind,
        )?;
        if source.kind == VirtualSourceKind::File && !new_filename.ends_with(".md") {
            return Err(MvError::UnsupportedVirtualTarget {
                path: destination_absolute,
                message: "virtual filesystem file moves currently require Markdown filenames"
                    .to_string(),
            });
        }

        return Ok(PreparedMove {
            mount: source_mount.clone(),
            source_absolute,
            destination_absolute,
            kind: PreparedMoveKind::VirtualFs(VirtualMoveRequest {
                identifier: source.identifier,
                new_parent_identifier: parent_identifier,
                new_filename,
                source_kind: source.kind,
            }),
        });
    }

    let parent = destination_absolute
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(PathBuf::new);
    if !path_is_directory(&parent) {
        return Err(MvError::MissingDestinationParent(parent));
    }
    if path_exists_or_symlink(&destination_absolute) {
        return Err(MvError::DestinationExists(destination_absolute));
    }

    Ok(PreparedMove {
        mount: source_mount.clone(),
        source_absolute,
        destination_absolute,
        kind: PreparedMoveKind::PlainFiles,
    })
}

fn execute_prepared_move_direct<S>(
    store: &mut S,
    prepared: &PreparedMove,
    state_root: Option<&Path>,
) -> Result<MvReport, MvError>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + VirtualMutationRepository
        + VirtualMoveRepository
        + FreshnessStateRepository,
{
    match &prepared.kind {
        PreparedMoveKind::PlainFiles => {
            fs::rename(&prepared.source_absolute, &prepared.destination_absolute).map_err(
                |error| MvError::Io {
                    path: prepared.source_absolute.clone(),
                    message: error.to_string(),
                },
            )?;
            Ok(report(prepared, "plain_files", None, None))
        }
        PreparedMoveKind::VirtualFs(request) => {
            let state_root = state_root.ok_or(MvError::VirtualStateRootRequired)?;
            let content_root = virtual_fs_content_root(state_root, &prepared.mount.mount_id);
            let mutation = rename_virtual_fs_item(
                store,
                &content_root,
                &prepared.mount.mount_id,
                &request.identifier,
                &request.new_parent_identifier,
                &request.new_filename,
            )
            .map_err(MvError::Locality)?;
            Ok(report_from_virtual_mutation(prepared, request, &mutation))
        }
    }
}

fn execute_virtual_move_with_daemon_or_direct(
    store: &mut SqliteStateStore,
    state_root: &Path,
    prepared: &PreparedMove,
    request: &VirtualMoveRequest,
) -> Result<MvReport, MvError> {
    if std::env::var("LOCALITY_DAEMON_DISABLE").is_ok() {
        return execute_prepared_move_direct(store, prepared, Some(state_root));
    }

    let daemon_request = DaemonRequest::VirtualFsRename {
        mount_id: prepared.mount.mount_id.0.clone(),
        identifier: request.identifier.clone(),
        new_parent_identifier: request.new_parent_identifier.clone(),
        new_filename: request.new_filename.clone(),
    };
    let response = match send_request_with_timeout(
        state_root,
        &daemon_request,
        daemon_mutating_request_timeout(),
    ) {
        Ok(response) => response,
        Err(DaemonClientError::NotAvailable(_)) => {
            return execute_prepared_move_direct(store, prepared, Some(state_root));
        }
        Err(DaemonClientError::TimedOut(_)) => {
            return Err(MvError::DaemonTimeout {
                timeout_ms: daemon_mutating_request_timeout().as_millis(),
            });
        }
        Err(error) => {
            return Err(MvError::DaemonError {
                code: "daemon_error".to_string(),
                message: error.message().to_string(),
            });
        }
    };
    if let Some(error) = response.error {
        return Err(MvError::DaemonError {
            code: error.code,
            message: error.message,
        });
    }
    let Some(payload) = response.payload else {
        return Err(MvError::DaemonError {
            code: "daemon_protocol_error".to_string(),
            message: "daemon returned no payload".to_string(),
        });
    };
    let mutation: VirtualFsMutationReport =
        serde_json::from_value(payload).map_err(|error| MvError::DaemonError {
            code: "daemon_protocol_error".to_string(),
            message: error.to_string(),
        })?;
    Ok(report_from_virtual_mutation(prepared, request, &mutation))
}

fn report_from_virtual_mutation(
    prepared: &PreparedMove,
    request: &VirtualMoveRequest,
    mutation: &VirtualFsMutationReport,
) -> MvReport {
    let destination = match request.source_kind {
        VirtualSourceKind::PageDirectory => prepared.mount.root.join(&mutation.item.path),
        VirtualSourceKind::File => prepared.mount.root.join(&mutation.item.path),
    };
    report(
        prepared,
        "virtual_fs",
        Some(destination),
        Some(mutation.identifier.clone()),
    )
}

fn report(
    prepared: &PreparedMove,
    mode: &str,
    destination_override: Option<PathBuf>,
    item_identifier: Option<String>,
) -> MvReport {
    let destination = destination_override.unwrap_or_else(|| prepared.destination_absolute.clone());
    let destination_display = destination.display().to_string();
    let next = vec![
        format!("loc diff {}", shell_quote_path(&destination_display)),
        format!("loc push {} -y", shell_quote_path(&destination_display)),
    ];
    MvReport {
        ok: true,
        command: "mv",
        action: "move",
        source: prepared.source_absolute.display().to_string(),
        destination: destination_display.clone(),
        mount_id: prepared.mount.mount_id.0.clone(),
        connector: prepared.mount.connector.clone(),
        projection: prepared.mount.projection.as_str().to_string(),
        mode: mode.to_string(),
        pushed: false,
        item_identifier,
        next,
        message: format!(
            "moved `{}` to `{}` locally; no remote changes were pushed",
            prepared.source_absolute.display(),
            destination.display()
        ),
    }
}

fn virtual_source_for_path<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
    absolute_path: &Path,
) -> Result<VirtualSource, MvError>
where
    S: EntityRepository + VirtualMutationRepository,
{
    let entities = store
        .list_entities(&mount.mount_id)
        .map_err(MvError::Store)?;
    let mutations = store
        .list_virtual_mutations(&mount.mount_id)
        .map_err(MvError::Store)?;

    for mutation in &mutations {
        if mutation.mutation_kind == VirtualMutationKind::Create
            && is_page_document_path(&mutation.projected_path)
            && page_container_path(&mutation.projected_path) == relative_path
        {
            return Ok(VirtualSource {
                identifier: format!("children:{}", mutation.local_id),
                kind: VirtualSourceKind::PageDirectory,
            });
        }
    }
    for entity in &entities {
        if entity.kind == EntityKind::Page
            && is_page_document_path(&entity.path)
            && page_container_path(&entity.path) == relative_path
        {
            return Ok(VirtualSource {
                identifier: format!("children:{}", entity.remote_id.0),
                kind: VirtualSourceKind::PageDirectory,
            });
        }
    }

    if let Some(mutation) = store
        .find_virtual_mutation_by_path(&mount.mount_id, relative_path)
        .map_err(MvError::Store)?
    {
        if mutation.mutation_kind == VirtualMutationKind::Create {
            return Ok(VirtualSource {
                identifier: mutation.local_id,
                kind: VirtualSourceKind::File,
            });
        }
        if let Some(remote_id) = mutation.target_remote_id {
            return Ok(VirtualSource {
                identifier: remote_id.0,
                kind: VirtualSourceKind::File,
            });
        }
    }

    if let Some(entity) = store
        .find_entity_by_path(&mount.mount_id, relative_path)
        .map_err(MvError::Store)?
    {
        return match entity.kind {
            EntityKind::Page => Ok(VirtualSource {
                identifier: entity.remote_id.0,
                kind: VirtualSourceKind::File,
            }),
            EntityKind::Database
            | EntityKind::Directory
            | EntityKind::Asset
            | EntityKind::Unknown(_) => Err(MvError::UnsupportedVirtualTarget {
                path: absolute_path.to_path_buf(),
                message: "only page directories and Markdown files can be moved through `loc mv`"
                    .to_string(),
            }),
        };
    }

    Err(MvError::MissingSource(absolute_path.to_path_buf()))
}

fn virtual_container_identifier_for_path<S>(
    store: &S,
    mount: &MountConfig,
    relative_path: &Path,
) -> Result<Option<String>, MvError>
where
    S: EntityRepository + VirtualMutationRepository,
{
    if relative_path.as_os_str().is_empty() {
        return Ok(Some(mount_point_identifier(mount)));
    }

    if let Some(entity) = store
        .find_entity_by_path(&mount.mount_id, relative_path)
        .map_err(MvError::Store)?
        && matches!(entity.kind, EntityKind::Database | EntityKind::Directory)
    {
        return Ok(Some(entity.remote_id.0));
    }

    for mutation in store
        .list_virtual_mutations(&mount.mount_id)
        .map_err(MvError::Store)?
    {
        if mutation.mutation_kind == VirtualMutationKind::Create
            && is_page_document_path(&mutation.projected_path)
            && page_container_path(&mutation.projected_path) == relative_path
        {
            return Ok(Some(format!("children:{}", mutation.local_id)));
        }
    }

    for entity in store
        .list_entities(&mount.mount_id)
        .map_err(MvError::Store)?
    {
        if entity.kind == EntityKind::Page
            && is_page_document_path(&entity.path)
            && page_container_path(&entity.path) == relative_path
        {
            return Ok(Some(format!("children:{}", entity.remote_id.0)));
        }
    }

    Ok(None)
}

fn validate_virtual_destination<S>(
    store: &S,
    mount: &MountConfig,
    destination_relative_path: &Path,
    source_kind: VirtualSourceKind,
) -> Result<(), MvError>
where
    S: EntityRepository + VirtualMutationRepository,
{
    if path_exists_or_symlink(&mount.root.join(destination_relative_path)) {
        return Err(MvError::DestinationExists(
            mount.root.join(destination_relative_path),
        ));
    }
    match source_kind {
        VirtualSourceKind::File => {
            if store
                .find_entity_by_path(&mount.mount_id, destination_relative_path)
                .map_err(MvError::Store)?
                .is_some()
                || store
                    .find_virtual_mutation_by_path(&mount.mount_id, destination_relative_path)
                    .map_err(MvError::Store)?
                    .is_some()
            {
                return Err(MvError::DestinationExists(
                    mount.root.join(destination_relative_path),
                ));
            }
        }
        VirtualSourceKind::PageDirectory => {
            let destination_page_path = page_document_path(destination_relative_path);
            if store
                .find_entity_by_path(&mount.mount_id, &destination_page_path)
                .map_err(MvError::Store)?
                .is_some()
                || store
                    .find_virtual_mutation_by_path(&mount.mount_id, &destination_page_path)
                    .map_err(MvError::Store)?
                    .is_some()
                || store
                    .find_entity_by_path(&mount.mount_id, destination_relative_path)
                    .map_err(MvError::Store)?
                    .is_some()
                || store
                    .find_virtual_mutation_by_path(&mount.mount_id, destination_relative_path)
                    .map_err(MvError::Store)?
                    .is_some()
            {
                return Err(MvError::DestinationExists(
                    mount.root.join(destination_relative_path),
                ));
            }
            for entity in store
                .list_entities(&mount.mount_id)
                .map_err(MvError::Store)?
            {
                if entity.kind == EntityKind::Page
                    && is_page_document_path(&entity.path)
                    && page_container_path(&entity.path) == destination_relative_path
                {
                    return Err(MvError::DestinationExists(
                        mount.root.join(destination_relative_path),
                    ));
                }
            }
            for mutation in store
                .list_virtual_mutations(&mount.mount_id)
                .map_err(MvError::Store)?
            {
                if mutation.mutation_kind == VirtualMutationKind::Create
                    && is_page_document_path(&mutation.projected_path)
                    && page_container_path(&mutation.projected_path) == destination_relative_path
                {
                    return Err(MvError::DestinationExists(
                        mount.root.join(destination_relative_path),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_filename(path: &Path) -> Result<(), MvError> {
    let Some(filename) = path.file_name() else {
        return Err(MvError::InvalidFilename {
            path: path.to_path_buf(),
            message: "destination must include a filename".to_string(),
        });
    };
    let filename_path = Path::new(filename);
    if filename_path.components().count() != 1 {
        return Err(MvError::InvalidFilename {
            path: path.to_path_buf(),
            message: "destination filename must be a single path component".to_string(),
        });
    }
    match filename_path.components().next() {
        Some(Component::Normal(name)) if !name.is_empty() => Ok(()),
        _ => Err(MvError::InvalidFilename {
            path: path.to_path_buf(),
            message: "destination filename must be a normal path component".to_string(),
        }),
    }
}

fn destination_filename_string(path: &Path) -> Result<String, MvError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .ok_or_else(|| MvError::InvalidFilename {
            path: path.to_path_buf(),
            message: "virtual filesystem filenames must be valid UTF-8".to_string(),
        })
}

fn parent_relative_path(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| *parent != Path::new(""))
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

fn absolute_path(path: &Path) -> Result<PathBuf, MvError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| MvError::CurrentDir {
                message: error.to_string(),
            })
    }
}

fn path_exists_or_symlink(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

fn path_is_directory(path: &Path) -> bool {
    path.metadata().is_ok_and(|metadata| metadata.is_dir())
}

fn daemon_mutating_request_timeout() -> Duration {
    std::env::var("LOCALITY_DAEMON_REQUEST_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_DAEMON_MUTATING_TIMEOUT)
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

impl From<io::Error> for MvError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            path: PathBuf::new(),
            message: error.to_string(),
        }
    }
}
