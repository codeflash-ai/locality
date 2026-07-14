use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{Seek, SeekFrom, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use fuse3::path::prelude::*;
use fuse3::{FileType, MountOptions, Result as FuseResult};
use futures_util::stream;
use locality_core::model::{EntityKind, MountId};
use locality_store::{MountConfig, MountRepository, ProjectionMode, SqliteStateStore};
use localityd::ipc::{DaemonRequest, DaemonResponse, send_request_with_timeout};
use localityd::virtual_fs::{
    ROOT_CONTAINER_IDENTIFIER, VirtualFsChildrenReport, VirtualFsItem, VirtualFsItemKind,
    VirtualFsItemReport, VirtualFsMaterializeReport, VirtualFsMutationReport, VirtualFsWriteReport,
};
use serde::de::DeserializeOwned;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

const ATTR_TTL: Duration = Duration::from_secs(1);
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(30);
const DAEMON_READY_POLL: Duration = Duration::from_millis(250);
const DAEMON_PING_TIMEOUT: Duration = Duration::from_secs(2);
const METADATA_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const MATERIALIZE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MUTATION_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const ROOT_PATH: &str = "/";
const DIRECTORY_METADATA_FILENAME: &str = ".directory";
const DIRECTORY_METADATA_IDENTIFIER: &str = "locality:shared-root-directory-metadata";
const DIRECTORY_METADATA_CONTENT: &str = "[Desktop Entry]\nIcon=locality-mount-logo\n";
const STAGING_HINT_MAX_BYTES: usize = 48;
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Debug)]
struct FuseOptions {
    mount_id: Option<String>,
    state_root: PathBuf,
    mountpoint: PathBuf,
}

pub(super) fn main() {
    if let Err(error) = run() {
        eprintln!("locality-fuse: {error}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn run() -> Result<(), String> {
    let options = parse_args(std::env::args().skip(1).collect())?;
    std::fs::create_dir_all(&options.mountpoint).map_err(|error| {
        format!(
            "failed to create mountpoint `{}`: {error}",
            options.mountpoint.display()
        )
    })?;
    let staging_id = staging_id_for_mount(options.mount_id.as_deref(), &options.mountpoint);
    std::fs::create_dir_all(staging_root(&options.state_root, &staging_id))
        .map_err(|error| format!("failed to create staging directory: {error}"))?;
    wait_for_daemon(&options.state_root).await?;

    let fs = AgentFuse::new(DaemonClient {
        state_root: options.state_root.clone(),
        mount_id: options.mount_id.clone(),
        mountpoint: options.mountpoint.clone(),
    });
    let mut mount_options = MountOptions::default();
    mount_options.fs_name(format!("locality:{staging_id}"));
    mount_options.nonempty(true);

    let handle = fuse3::path::Session::new(mount_options)
        .mount_with_unprivileged(fs, &options.mountpoint)
        .await
        .map_err(|error| {
            format!(
                "failed to mount `{}`: {error}",
                options.mountpoint.display()
            )
        })?;

    wait_for_shutdown()
        .await
        .map_err(|error| format!("failed to wait for shutdown signal: {error}"))?;
    handle.unmount().await.map_err(|error| {
        format!(
            "failed to unmount `{}`: {error}",
            options.mountpoint.display()
        )
    })
}

async fn wait_for_daemon(state_root: &Path) -> Result<(), String> {
    let started = Instant::now();
    let mut last_error = String::new();

    while started.elapsed() < DAEMON_READY_TIMEOUT {
        match ping_daemon(state_root) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = error,
        }
        tokio::time::sleep(DAEMON_READY_POLL).await;
    }

    Err(format!(
        "localityd did not become ready within {}s: {last_error}",
        DAEMON_READY_TIMEOUT.as_secs()
    ))
}

fn ping_daemon(state_root: &Path) -> Result<(), String> {
    let response = send_request_with_timeout(state_root, &DaemonRequest::Ping, DAEMON_PING_TIMEOUT)
        .map_err(|error| error.message().to_string())?;
    daemon_ping_result(response)
}

fn daemon_ping_result(response: DaemonResponse) -> Result<(), String> {
    if response.ok {
        return Ok(());
    }

    match response.error {
        Some(error) => Err(format!("{}: {}", error.code, error.message)),
        None => Err("daemon ping returned a failure without an error message".to_string()),
    }
}

#[cfg(unix)]
async fn wait_for_shutdown() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

fn parse_args(args: Vec<String>) -> Result<FuseOptions, String> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Err(
            "usage: locality-fuse [--mount-id <id>] --state-dir <path> --mountpoint <path>"
                .to_string(),
        );
    }

    let mount_id = flag_value(&args, "--mount-id").map(str::to_string);
    let state_root = flag_value(&args, "--state-dir")
        .map(PathBuf::from)
        .unwrap_or_else(default_state_root);
    let mountpoint = flag_value(&args, "--mountpoint")
        .ok_or_else(|| "locality-fuse requires --mountpoint <path>".to_string())
        .map(PathBuf::from)?;

    Ok(FuseOptions {
        mount_id,
        state_root,
        mountpoint,
    })
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn default_state_root() -> PathBuf {
    std::env::var("LOCALITY_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|home| PathBuf::from(home).join(".loc")))
        .unwrap_or_else(|_| PathBuf::from(".loc"))
}

fn staging_root(state_root: &Path, mount_id: &str) -> PathBuf {
    state_root.join("fuse-staging").join(mount_id)
}

fn staging_id_for_mount(mount_id: Option<&str>, mountpoint: &Path) -> String {
    match mount_id {
        Some(mount_id) => mount_id.to_string(),
        None => {
            let hint = bounded_staging_hint(mountpoint, STAGING_HINT_MAX_BYTES);
            format!("root-{hint}-{}", stable_path_hash(mountpoint))
        }
    }
}

fn bounded_staging_hint(path: &Path, max_bytes: usize) -> String {
    let sanitized = path
        .display()
        .to_string()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let mut hint = String::with_capacity(max_bytes.min(sanitized.len()));
    for character in sanitized.chars() {
        if hint.len() + character.len_utf8() > max_bytes {
            break;
        }
        hint.push(character);
    }
    let hint = hint.trim_matches('_');
    if hint.is_empty() {
        "root".to_string()
    } else {
        hint.to_string()
    }
}

#[cfg(unix)]
fn stable_path_hash(path: &Path) -> String {
    stable_hex_hash(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn stable_path_hash(path: &Path) -> String {
    stable_hex_hash(path.display().to_string().as_bytes())
}

fn stable_hex_hash(bytes: &[u8]) -> String {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

fn projection_root_identifier(mount_id: &str) -> String {
    format!("mount:{mount_id}")
}

fn directory_metadata_item() -> VirtualFsItem {
    VirtualFsItem {
        identifier: DIRECTORY_METADATA_IDENTIFIER.to_string(),
        parent_identifier: Some(ROOT_CONTAINER_IDENTIFIER.to_string()),
        filename: DIRECTORY_METADATA_FILENAME.to_string(),
        kind: VirtualFsItemKind::File,
        read_only: true,
        entity_kind: None,
        remote_id: None,
        path: DIRECTORY_METADATA_FILENAME.to_string(),
        hydration: None,
        content_type: "application/x-desktop".to_string(),
        remote_edited_at: None,
        materialized_path: None,
        byte_size: Some(DIRECTORY_METADATA_CONTENT.len() as u64),
    }
}

fn virtual_file_contents(identifier: &str) -> Option<&'static [u8]> {
    match identifier {
        DIRECTORY_METADATA_IDENTIFIER => Some(DIRECTORY_METADATA_CONTENT.as_bytes()),
        _ => None,
    }
}

fn is_shared_root_item(item: &VirtualFsItem) -> bool {
    item.kind == VirtualFsItemKind::Folder
        && item.identifier == ROOT_CONTAINER_IDENTIFIER
        && item.path.is_empty()
}

fn directory_listing_items_for_parent(
    parent_path: &Path,
    parent: &VirtualFsItem,
    children: Vec<VirtualFsItem>,
) -> Vec<VirtualFsItem> {
    let mut items = Vec::with_capacity(children.len() + 1);
    if normalize_path(parent_path) == Path::new(ROOT_PATH) && is_shared_root_item(parent) {
        items.push(directory_metadata_item());
    }
    items.extend(children);
    items
}

fn read_virtual_bytes(identifier: &str, offset: u64, size: u32) -> Option<Bytes> {
    let bytes = virtual_file_contents(identifier)?;
    let start = offset.min(bytes.len() as u64) as usize;
    let end = (start + size as usize).min(bytes.len());
    Some(Bytes::copy_from_slice(&bytes[start..end]))
}

#[derive(Clone, Debug)]
struct DaemonClient {
    state_root: PathBuf,
    mount_id: Option<String>,
    mountpoint: PathBuf,
}

trait VirtualFsClient {
    fn state_root(&self) -> &Path;

    fn staging_id(&self) -> String;

    fn root_identifier(&self) -> String;

    fn item(&self, identifier: &str) -> Result<VirtualFsItemReport, FuseError>;

    fn children(&self, container_identifier: &str) -> Result<VirtualFsChildrenReport, FuseError>;

    fn materialize(&self, identifier: &str) -> Result<VirtualFsMaterializeReport, FuseError>;

    fn commit_write(
        &self,
        identifier: &str,
        bytes: Vec<u8>,
    ) -> Result<VirtualFsWriteReport, FuseError>;

    fn create_file(
        &self,
        parent_identifier: &str,
        filename: &str,
    ) -> Result<VirtualFsMutationReport, FuseError>;

    fn create_directory(
        &self,
        parent_identifier: &str,
        dirname: &str,
    ) -> Result<VirtualFsMutationReport, FuseError>;

    fn rename(
        &self,
        identifier: &str,
        new_parent_identifier: &str,
        new_filename: &str,
    ) -> Result<VirtualFsMutationReport, FuseError>;

    fn trash(&self, identifier: &str) -> Result<VirtualFsMutationReport, FuseError>;
}

impl VirtualFsClient for DaemonClient {
    fn state_root(&self) -> &Path {
        &self.state_root
    }

    fn staging_id(&self) -> String {
        staging_id_for_mount(self.mount_id.as_deref(), &self.mountpoint)
    }

    fn root_identifier(&self) -> String {
        match &self.mount_id {
            Some(mount_id) => projection_root_identifier(mount_id),
            None => ROOT_CONTAINER_IDENTIFIER.to_string(),
        }
    }

    fn item(&self, identifier: &str) -> Result<VirtualFsItemReport, FuseError> {
        if self.mount_id.is_none() && identifier == ROOT_CONTAINER_IDENTIFIER {
            return Ok(VirtualFsItemReport {
                mount_id: String::new(),
                item: self.shared_root_item(),
            });
        }

        let route = self.route_identifier(identifier)?;
        let mut report = self.request_with_timeout(
            &DaemonRequest::VirtualFsItem {
                mount_id: route.mount_id.0.clone(),
                identifier: route.daemon_identifier.clone(),
            },
            METADATA_REQUEST_TIMEOUT,
        )?;
        self.wrap_item_report(&route, &mut report);
        Ok(report)
    }

    fn children(&self, container_identifier: &str) -> Result<VirtualFsChildrenReport, FuseError> {
        if self.mount_id.is_none() && container_identifier == ROOT_CONTAINER_IDENTIFIER {
            return self.request_with_timeout(
                &DaemonRequest::VirtualProjectionRootChildren {
                    projection_root: self.mountpoint.clone(),
                    projection: ProjectionMode::LinuxFuse,
                },
                METADATA_REQUEST_TIMEOUT,
            );
        }

        let route = self.route_identifier(container_identifier)?;
        let mut report = self.request_with_timeout(
            &DaemonRequest::VirtualFsChildren {
                mount_id: route.mount_id.0.clone(),
                container_identifier: route.daemon_identifier.clone(),
            },
            METADATA_REQUEST_TIMEOUT,
        )?;
        self.wrap_children_report(&route, container_identifier, &mut report);
        Ok(report)
    }

    fn materialize(&self, identifier: &str) -> Result<VirtualFsMaterializeReport, FuseError> {
        let route = self.route_identifier(identifier)?;
        let mut report = self.request_with_timeout(
            &DaemonRequest::VirtualFsMaterialize {
                mount_id: route.mount_id.0.clone(),
                identifier: route.daemon_identifier.clone(),
            },
            MATERIALIZE_REQUEST_TIMEOUT,
        )?;
        self.wrap_materialize_report(&route, &mut report);
        Ok(report)
    }

    fn commit_write(
        &self,
        identifier: &str,
        bytes: Vec<u8>,
    ) -> Result<VirtualFsWriteReport, FuseError> {
        let route = self.route_identifier(identifier)?;
        let mut report = self.request_with_timeout(
            &DaemonRequest::VirtualFsCommitWrite {
                mount_id: route.mount_id.0.clone(),
                identifier: route.daemon_identifier.clone(),
                contents_base64: BASE64.encode(bytes),
            },
            MUTATION_REQUEST_TIMEOUT,
        )?;
        self.wrap_write_report(&route, &mut report);
        Ok(report)
    }

    fn create_file(
        &self,
        parent_identifier: &str,
        filename: &str,
    ) -> Result<VirtualFsMutationReport, FuseError> {
        let route = self.route_identifier(parent_identifier)?;
        let mut report = self.request_with_timeout(
            &DaemonRequest::VirtualFsCreateFile {
                mount_id: route.mount_id.0.clone(),
                parent_identifier: route.daemon_identifier.clone(),
                filename: filename.to_string(),
            },
            MUTATION_REQUEST_TIMEOUT,
        )?;
        self.wrap_mutation_report(&route, &mut report);
        Ok(report)
    }

    fn create_directory(
        &self,
        parent_identifier: &str,
        dirname: &str,
    ) -> Result<VirtualFsMutationReport, FuseError> {
        let route = self.route_identifier(parent_identifier)?;
        let mut report = self.request_with_timeout(
            &DaemonRequest::VirtualFsCreateDirectory {
                mount_id: route.mount_id.0.clone(),
                parent_identifier: route.daemon_identifier.clone(),
                dirname: dirname.to_string(),
            },
            MUTATION_REQUEST_TIMEOUT,
        )?;
        self.wrap_mutation_report(&route, &mut report);
        Ok(report)
    }

    fn rename(
        &self,
        identifier: &str,
        new_parent_identifier: &str,
        new_filename: &str,
    ) -> Result<VirtualFsMutationReport, FuseError> {
        let route = self.route_identifier(identifier)?;
        let parent_route = self.route_identifier(new_parent_identifier)?;
        if route.mount_id != parent_route.mount_id {
            return Err(FuseError::Invalid);
        }
        let mut report = self.request_with_timeout(
            &DaemonRequest::VirtualFsRename {
                mount_id: route.mount_id.0.clone(),
                identifier: route.daemon_identifier.clone(),
                new_parent_identifier: parent_route.daemon_identifier.clone(),
                new_filename: new_filename.to_string(),
            },
            MUTATION_REQUEST_TIMEOUT,
        )?;
        self.wrap_mutation_report(&route, &mut report);
        Ok(report)
    }

    fn trash(&self, identifier: &str) -> Result<VirtualFsMutationReport, FuseError> {
        let route = self.route_identifier(identifier)?;
        let mut report = self.request_with_timeout(
            &DaemonRequest::VirtualFsTrash {
                mount_id: route.mount_id.0.clone(),
                identifier: route.daemon_identifier.clone(),
            },
            MUTATION_REQUEST_TIMEOUT,
        )?;
        self.wrap_mutation_report(&route, &mut report);
        Ok(report)
    }
}

impl DaemonClient {
    fn route_identifier(&self, identifier: &str) -> Result<RoutedIdentifier, FuseError> {
        if let Some(mount_id) = &self.mount_id {
            return Ok(RoutedIdentifier {
                mount_id: MountId::new(mount_id.clone()),
                daemon_identifier: identifier.to_string(),
                mount: None,
            });
        }

        let shared =
            localityd::virtual_projection::unwrap_identifier(identifier).map_err(|error| {
                FuseError::Daemon(format!(
                    "invalid shared projection identifier `{identifier}`: {error}"
                ))
            })?;
        let mount = self.load_mount_config(&shared.mount_id)?;
        Ok(RoutedIdentifier {
            mount_id: shared.mount_id,
            daemon_identifier: shared.daemon_identifier,
            mount: Some(mount),
        })
    }

    fn load_mount_config(&self, mount_id: &MountId) -> Result<MountConfig, FuseError> {
        let store = SqliteStateStore::open(self.state_root.clone()).map_err(|error| {
            FuseError::Daemon(format!(
                "failed to open state store `{}`: {error}",
                self.state_root.display()
            ))
        })?;
        let mount = store
            .get_mount(mount_id)
            .map_err(|error| {
                FuseError::Daemon(format!("failed to load mount `{}`: {error}", mount_id.0))
            })?
            .ok_or(FuseError::NotFound)?;
        if mount.projection != ProjectionMode::LinuxFuse
            || localityd::virtual_fs::virtual_projection_root(&mount) != self.mountpoint
        {
            return Err(FuseError::NotFound);
        }
        Ok(mount)
    }

    fn shared_root_item(&self) -> VirtualFsItem {
        VirtualFsItem {
            identifier: ROOT_CONTAINER_IDENTIFIER.to_string(),
            parent_identifier: None,
            filename: String::new(),
            kind: VirtualFsItemKind::Folder,
            read_only: false,
            entity_kind: None,
            remote_id: None,
            path: String::new(),
            hydration: None,
            content_type: "public.folder".to_string(),
            remote_edited_at: None,
            materialized_path: Some(self.mountpoint.display().to_string()),
            byte_size: None,
        }
    }

    fn wrap_item_report(&self, route: &RoutedIdentifier, report: &mut VirtualFsItemReport) {
        if let Some(mount) = &route.mount {
            report.item = localityd::virtual_projection::wrap_item(mount, report.item.clone());
        }
    }

    fn wrap_children_report(
        &self,
        route: &RoutedIdentifier,
        shared_container_identifier: &str,
        report: &mut VirtualFsChildrenReport,
    ) {
        if let Some(mount) = &route.mount {
            report.container_identifier = shared_container_identifier.to_string();
            report.children = report
                .children
                .iter()
                .cloned()
                .map(|item| localityd::virtual_projection::wrap_item(mount, item))
                .collect();
        }
    }

    fn wrap_materialize_report(
        &self,
        route: &RoutedIdentifier,
        report: &mut VirtualFsMaterializeReport,
    ) {
        if route.mount.is_some() {
            report.identifier =
                localityd::virtual_projection::wrap_identifier(&route.mount_id, &report.identifier);
        }
    }

    fn wrap_write_report(&self, route: &RoutedIdentifier, report: &mut VirtualFsWriteReport) {
        if route.mount.is_some() {
            report.identifier =
                localityd::virtual_projection::wrap_identifier(&route.mount_id, &report.identifier);
        }
    }

    fn wrap_mutation_report(&self, route: &RoutedIdentifier, report: &mut VirtualFsMutationReport) {
        if let Some(mount) = &route.mount {
            report.item = localityd::virtual_projection::wrap_item(mount, report.item.clone());
            report.identifier = report.item.identifier.clone();
            report.path = report.item.path.clone();
        }
    }

    fn request_with_timeout<T>(
        &self,
        request: &DaemonRequest,
        timeout: Duration,
    ) -> Result<T, FuseError>
    where
        T: DeserializeOwned,
    {
        let response = send_request_with_timeout(&self.state_root, request, timeout)
            .map_err(|error| FuseError::Daemon(error.message().to_string()))?;
        decode_response(response)
    }
}

#[derive(Clone, Debug)]
struct RoutedIdentifier {
    mount_id: MountId,
    daemon_identifier: String,
    mount: Option<MountConfig>,
}

fn decode_response<T>(response: DaemonResponse) -> Result<T, FuseError>
where
    T: DeserializeOwned,
{
    if let Some(error) = response.error {
        if error.code == "remote_not_found"
            || (error.code == "invalid_state"
                && error.message.contains("not present in daemon state"))
        {
            return Err(FuseError::NotFound);
        }
        if error.code == "unsupported" {
            return Err(FuseError::Invalid);
        }
        return Err(FuseError::Daemon(format!(
            "{}: {}",
            error.code, error.message
        )));
    }
    let payload = response
        .payload
        .ok_or_else(|| FuseError::Daemon("daemon returned no payload".to_string()))?;
    serde_json::from_value(payload).map_err(|error| FuseError::Daemon(error.to_string()))
}

#[derive(Debug)]
enum FuseError {
    Daemon(String),
    Io(String),
    NotFound,
    NotFile,
    NotDirectory,
    ReadOnly,
    Invalid,
}

impl FuseError {
    fn is_remote_missing(&self) -> bool {
        match self {
            Self::NotFound => true,
            Self::Daemon(message) => daemon_error_is_remote_missing(message),
            Self::Io(_) | Self::NotFile | Self::NotDirectory | Self::ReadOnly | Self::Invalid => {
                false
            }
        }
    }
}

fn daemon_error_is_remote_missing(message: &str) -> bool {
    message.starts_with("remote_not_found:")
        || (message.starts_with("invalid_state:")
            && message.contains("not present in daemon state"))
}

impl From<FuseError> for fuse3::Errno {
    fn from(error: FuseError) -> Self {
        match error {
            FuseError::NotFound => libc::ENOENT.into(),
            FuseError::NotFile => libc::EISDIR.into(),
            FuseError::NotDirectory => libc::ENOTDIR.into(),
            FuseError::ReadOnly => libc::EROFS.into(),
            FuseError::Invalid => libc::EINVAL.into(),
            FuseError::Daemon(message) | FuseError::Io(message) => {
                eprintln!("locality-fuse: {message}");
                libc::EIO.into()
            }
        }
    }
}

struct AgentFuse<C = DaemonClient> {
    client: C,
    cache: Mutex<BTreeMap<PathBuf, VirtualFsItem>>,
    handles: Mutex<BTreeMap<u64, OpenHandle>>,
    next_handle: AtomicU64,
}

#[derive(Debug)]
struct OpenHandle {
    identifier: String,
    path: PathBuf,
    writable: bool,
    dirty: bool,
    temp_path: Option<PathBuf>,
}

impl<C> AgentFuse<C>
where
    C: VirtualFsClient,
{
    fn new(client: C) -> Self {
        let mut cache = BTreeMap::new();
        if let Ok(report) = client.item(&client.root_identifier()) {
            cache.insert(PathBuf::from(ROOT_PATH), report.item);
        }
        Self {
            client,
            cache: Mutex::new(cache),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        }
    }

    fn root_item(&self) -> Result<VirtualFsItem, FuseError> {
        if let Some(item) = self
            .cache
            .lock()
            .expect("fuse item cache")
            .get(Path::new(ROOT_PATH))
            .cloned()
        {
            return Ok(item);
        }

        let report = self.client.item(&self.client.root_identifier())?;
        self.cache_item_at(PathBuf::from(ROOT_PATH), report.item.clone());
        Ok(report.item)
    }

    fn resolve_path(&self, path: &Path) -> Result<VirtualFsItem, FuseError> {
        let path = normalize_path(path);
        if path == Path::new(ROOT_PATH) {
            return self.root_item();
        }
        if path == child_path(Path::new(ROOT_PATH), DIRECTORY_METADATA_FILENAME) {
            let root = self.root_item()?;
            if is_shared_root_item(&root) {
                return Ok(directory_metadata_item());
            }
        }
        let cached = {
            self.cache
                .lock()
                .expect("fuse item cache")
                .get(&path)
                .cloned()
        };
        if let Some(item) = cached {
            if is_local_cached_identifier(&item.identifier) {
                if let Ok(report) = self.client.item(&item.identifier) {
                    self.cache_item_at(path.clone(), report.item.clone());
                    return Ok(report.item);
                }
                self.remove_cached_path(&path);
            } else {
                return Ok(item);
            }
        }
        let parent = path.parent().unwrap_or_else(|| Path::new(ROOT_PATH));
        let filename = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or(FuseError::NotFound)?;
        let parent_item = self.resolve_path(parent)?;
        if parent_item.kind != VirtualFsItemKind::Folder {
            return Err(FuseError::NotDirectory);
        }
        let children = match self.client.children(&parent_item.identifier) {
            Ok(children) => children,
            Err(error) if error.is_remote_missing() => {
                self.remove_cached_path(parent);
                return Err(FuseError::NotFound);
            }
            Err(error) => return Err(error),
        };
        let mut found = None;
        let mut cache = self.cache.lock().expect("fuse item cache");
        for child in children.children {
            let child_path = child_path(parent, &child.filename);
            if child.filename == filename {
                found = Some(child.clone());
            }
            cache.insert(child_path, child);
        }
        found.ok_or(FuseError::NotFound)
    }

    fn materialized_path(&self, item: &VirtualFsItem) -> Result<PathBuf, FuseError> {
        if item.kind != VirtualFsItemKind::File {
            return Err(FuseError::NotFile);
        }
        let report = match self.client.materialize(&item.identifier) {
            Ok(report) => report,
            Err(error) if error.is_remote_missing() => {
                self.remove_cached_identifier(&item.identifier);
                return Err(FuseError::NotFound);
            }
            Err(error) => return Err(error),
        };
        self.update_cached_materialized_path(&report.identifier, &report.path);
        Ok(PathBuf::from(report.path))
    }

    fn trash_path(&self, path: &Path, expected_kind: VirtualFsItemKind) -> Result<(), FuseError> {
        let path = normalize_path(path);
        let cached_pending_folder = expected_kind == VirtualFsItemKind::Folder
            && self
                .cache
                .lock()
                .expect("fuse item cache")
                .get(&path)
                .is_some_and(|item| {
                    item.kind == VirtualFsItemKind::Folder
                        && is_local_cached_identifier(&item.identifier)
                });
        let item = match self.resolve_path(&path) {
            Ok(item) => item,
            Err(FuseError::NotFound) if cached_pending_folder => {
                self.remove_cached_path(&path);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if item.kind != expected_kind {
            return Err(match expected_kind {
                VirtualFsItemKind::File => FuseError::NotFile,
                VirtualFsItemKind::Folder => FuseError::NotDirectory,
            });
        }
        ensure_writable_item(&item)?;
        self.client.trash(&item.identifier)?;
        self.remove_cached_path(&path);
        Ok(())
    }

    fn rename_path(
        &self,
        old_path: &Path,
        new_parent_path: &Path,
        filename: &str,
    ) -> Result<(), FuseError> {
        let item = self.resolve_path(old_path)?;
        ensure_writable_item(&item)?;
        let new_parent = self.resolve_path(new_parent_path)?;
        ensure_creatable_parent(&new_parent)?;
        let report = self
            .client
            .rename(&item.identifier, &new_parent.identifier, filename)?;
        self.remove_cached_path(old_path);
        self.cache_item_at(child_path(new_parent_path, filename), report.item);
        Ok(())
    }

    fn create_file_at_parent_path(
        &self,
        parent: &Path,
        filename: &str,
    ) -> Result<VirtualFsMutationReport, FuseError> {
        let parent_item = self.resolve_path(parent)?;
        ensure_creatable_parent(&parent_item)?;
        let report = self.client.create_file(&parent_item.identifier, filename)?;
        self.cache_item_at(child_path(parent, filename), report.item.clone());
        Ok(report)
    }

    fn update_cached_materialized_path(&self, identifier: &str, materialized_path: &str) {
        let mut cache = self.cache.lock().expect("fuse item cache");
        let byte_size = std::fs::metadata(materialized_path)
            .ok()
            .map(|metadata| metadata.len());
        for item in cache.values_mut() {
            if item.identifier == identifier {
                item.materialized_path = Some(materialized_path.to_string());
                item.byte_size = byte_size;
            }
        }
    }

    fn cache_item_at(&self, path: PathBuf, item: VirtualFsItem) {
        self.cache
            .lock()
            .expect("fuse item cache")
            .insert(normalize_path(&path), item);
    }

    fn remove_cached_path(&self, path: &Path) {
        let path = normalize_path(path);
        self.cache
            .lock()
            .expect("fuse item cache")
            .retain(|cached_path, _| cached_path != &path && !cached_path.starts_with(&path));
    }

    fn remove_cached_identifier(&self, identifier: &str) {
        self.cache
            .lock()
            .expect("fuse item cache")
            .retain(|_, item| item.identifier != identifier);
    }

    fn open_handle(&self, fh: u64) -> Result<OpenHandle, FuseError> {
        self.handles
            .lock()
            .expect("fuse handles")
            .get(&fh)
            .map(|handle| OpenHandle {
                identifier: handle.identifier.clone(),
                path: handle.path.clone(),
                writable: handle.writable,
                dirty: handle.dirty,
                temp_path: handle.temp_path.clone(),
            })
            .ok_or(FuseError::Invalid)
    }

    fn item_by_identifier(&self, identifier: &str) -> Result<VirtualFsItem, FuseError> {
        if let Some(item) = self
            .cache
            .lock()
            .expect("fuse item cache")
            .values()
            .find(|item| item.identifier == identifier)
            .cloned()
        {
            return Ok(item);
        }
        let report = self.client.item(identifier)?;
        let item_path = if report.item.path.is_empty() {
            PathBuf::from(ROOT_PATH)
        } else {
            normalize_path(Path::new(&report.item.path))
        };
        self.cache
            .lock()
            .expect("fuse item cache")
            .insert(item_path, report.item.clone());
        Ok(report.item)
    }

    fn commit_handle(&self, fh: u64) -> Result<(), FuseError> {
        let (identifier, temp_path, dirty) = {
            let handles = self.handles.lock().expect("fuse handles");
            let handle = handles.get(&fh).ok_or(FuseError::Invalid)?;
            (
                handle.identifier.clone(),
                handle.temp_path.clone(),
                handle.dirty,
            )
        };
        if !dirty {
            return Ok(());
        }
        let Some(temp_path) = temp_path else {
            return Err(FuseError::Invalid);
        };
        let bytes = std::fs::read(&temp_path)
            .map_err(|error| FuseError::Io(format!("failed to read staged write: {error}")))?;
        let report = self.client.commit_write(&identifier, bytes)?;
        self.update_cached_materialized_path(&report.identifier, &report.path);
        if let Some(handle) = self.handles.lock().expect("fuse handles").get_mut(&fh) {
            handle.dirty = false;
        }
        Ok(())
    }
}

fn is_local_cached_identifier(identifier: &str) -> bool {
    let daemon_identifier = localityd::virtual_projection::unwrap_identifier(identifier)
        .map(|shared| shared.daemon_identifier)
        .unwrap_or_else(|_| identifier.to_string());
    daemon_identifier.starts_with("local:") || daemon_identifier.starts_with("children:local:")
}

impl<C> PathFilesystem for AgentFuse<C>
where
    C: VirtualFsClient + Send + Sync + 'static,
{
    async fn init(&self, _req: Request) -> FuseResult<ReplyInit> {
        Ok(ReplyInit {
            max_write: NonZeroU32::new(1024 * 1024).expect("max write is non-zero"),
        })
    }

    async fn destroy(&self, _req: Request) {}

    async fn lookup(&self, _req: Request, parent: &OsStr, name: &OsStr) -> FuseResult<ReplyEntry> {
        let path = child_path(Path::new(parent), &name.to_string_lossy());
        let item = self.resolve_path(&path)?;
        Ok(ReplyEntry {
            ttl: ATTR_TTL,
            attr: attr_for_item(&item),
        })
    }

    async fn getattr(
        &self,
        _req: Request,
        path: Option<&OsStr>,
        _fh: Option<u64>,
        _flags: u32,
    ) -> FuseResult<ReplyAttr> {
        let path = path.map(Path::new).unwrap_or_else(|| Path::new(ROOT_PATH));
        let item = self.resolve_path(path)?;
        Ok(ReplyAttr {
            ttl: ATTR_TTL,
            attr: attr_for_item(&item),
        })
    }

    async fn open(&self, _req: Request, path: &OsStr, flags: u32) -> FuseResult<ReplyOpen> {
        let item = self.resolve_path(Path::new(path))?;
        if item.kind != VirtualFsItemKind::File {
            return Err(FuseError::NotFile.into());
        }
        let writable = open_is_writable(flags);
        if writable {
            ensure_writable_item(&item)?;
        }
        if virtual_file_contents(&item.identifier).is_some() {
            let fh = self.next_handle.fetch_add(1, Ordering::Relaxed);
            self.handles.lock().expect("fuse handles").insert(
                fh,
                OpenHandle {
                    identifier: item.identifier,
                    path: PathBuf::new(),
                    writable: false,
                    dirty: false,
                    temp_path: None,
                },
            );
            return Ok(ReplyOpen { fh, flags: 0 });
        }

        let truncating = flags & libc::O_TRUNC as u32 != 0;
        let materialized = if truncating && writable {
            PathBuf::new()
        } else {
            self.materialized_path(&item)?
        };
        let fh = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let mut handle = OpenHandle {
            identifier: item.identifier,
            path: materialized.clone(),
            writable,
            dirty: false,
            temp_path: None,
        };

        if writable {
            let staging_id = self.client.staging_id();
            let temp_path =
                staging_root(self.client.state_root(), &staging_id).join(format!("{fh}.tmp"));
            if truncating {
                std::fs::write(&temp_path, []).map_err(|error| {
                    FuseError::Io(format!("failed to create write stage: {error}"))
                })?;
            } else {
                let bytes = std::fs::read(&materialized).map_err(|error| {
                    FuseError::Io(format!("failed to seed write stage: {error}"))
                })?;
                std::fs::write(&temp_path, bytes).map_err(|error| {
                    FuseError::Io(format!("failed to seed write stage: {error}"))
                })?;
            }
            handle.path = temp_path.clone();
            handle.temp_path = Some(temp_path);
        }

        self.handles
            .lock()
            .expect("fuse handles")
            .insert(fh, handle);
        Ok(ReplyOpen { fh, flags: 0 })
    }

    async fn read(
        &self,
        _req: Request,
        path: Option<&OsStr>,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> FuseResult<ReplyData> {
        let file_path = if fh != 0 {
            let handle = self.open_handle(fh)?;
            if let Some(data) = read_virtual_bytes(&handle.identifier, offset, size) {
                return Ok(ReplyData { data });
            }
            handle.path
        } else {
            let path = path.ok_or(FuseError::NotFound)?;
            let item = self.resolve_path(Path::new(path))?;
            if let Some(data) = read_virtual_bytes(&item.identifier, offset, size) {
                return Ok(ReplyData { data });
            }
            self.materialized_path(&item)?
        };
        let bytes = std::fs::read(&file_path).map_err(|error| {
            FuseError::Io(format!("failed to read `{}`: {error}", file_path.display()))
        })?;
        let start = offset.min(bytes.len() as u64) as usize;
        let end = (start + size as usize).min(bytes.len());
        Ok(ReplyData {
            data: Bytes::copy_from_slice(&bytes[start..end]),
        })
    }

    async fn write(
        &self,
        _req: Request,
        _path: Option<&OsStr>,
        fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
        _flags: u32,
    ) -> FuseResult<ReplyWrite> {
        let handle = self.open_handle(fh)?;
        if !handle.writable {
            return Err(FuseError::ReadOnly.into());
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&handle.path)
            .map_err(|error| FuseError::Io(format!("failed to open write stage: {error}")))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| FuseError::Io(format!("failed to seek write stage: {error}")))?;
        file.write_all(data)
            .map_err(|error| FuseError::Io(format!("failed to write stage: {error}")))?;
        if let Some(handle) = self.handles.lock().expect("fuse handles").get_mut(&fh) {
            handle.dirty = true;
        }
        Ok(ReplyWrite {
            written: data.len() as u32,
        })
    }

    async fn setattr(
        &self,
        _req: Request,
        path: Option<&OsStr>,
        fh: Option<u64>,
        set_attr: fuse3::SetAttr,
    ) -> FuseResult<ReplyAttr> {
        if let Some(size) = set_attr.size {
            if let Some(fh) = fh {
                let handle = self.open_handle(fh)?;
                if !handle.writable {
                    return Err(FuseError::ReadOnly.into());
                }
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&handle.path)
                    .map_err(|error| {
                        FuseError::Io(format!("failed to truncate write stage: {error}"))
                    })?;
                file.set_len(size).map_err(|error| {
                    FuseError::Io(format!("failed to truncate write stage: {error}"))
                })?;
                if let Some(handle) = self.handles.lock().expect("fuse handles").get_mut(&fh) {
                    handle.dirty = true;
                }
            } else {
                let path = path.ok_or(FuseError::NotFound)?;
                let item = self.resolve_path(Path::new(path))?;
                ensure_writable_item(&item)?;
                let materialized = self.materialized_path(&item)?;
                let mut bytes = std::fs::read(&materialized).map_err(|error| {
                    FuseError::Io(format!("failed to read materialized file: {error}"))
                })?;
                bytes.resize(size as usize, 0);
                let report = self.client.commit_write(&item.identifier, bytes)?;
                self.update_cached_materialized_path(&report.identifier, &report.path);
            }
        }
        let item = if let Some(path) = path {
            self.resolve_path(Path::new(path))?
        } else if let Some(fh) = fh {
            let handle = self.open_handle(fh)?;
            self.item_by_identifier(&handle.identifier)?
        } else {
            return Err(FuseError::NotFound.into());
        };
        let mut attr = attr_for_item(&item);
        if let Some(fh) = fh
            && let Ok(handle) = self.open_handle(fh)
            && let Ok(metadata) = std::fs::metadata(&handle.path)
        {
            attr.size = metadata.len();
            attr.blocks = attr.size.div_ceil(512);
        }
        Ok(ReplyAttr {
            ttl: ATTR_TTL,
            attr,
        })
    }

    async fn flush(
        &self,
        _req: Request,
        _path: Option<&OsStr>,
        fh: u64,
        _lock_owner: u64,
    ) -> FuseResult<()> {
        self.commit_handle(fh)?;
        Ok(())
    }

    async fn fsync(
        &self,
        _req: Request,
        _path: Option<&OsStr>,
        fh: u64,
        _datasync: bool,
    ) -> FuseResult<()> {
        self.commit_handle(fh)?;
        Ok(())
    }

    async fn release(
        &self,
        _req: Request,
        _path: Option<&OsStr>,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        flush: bool,
    ) -> FuseResult<()> {
        if flush {
            self.commit_handle(fh)?;
        }
        let handle = self.handles.lock().expect("fuse handles").remove(&fh);
        if let Some(handle) = handle.and_then(|handle| handle.temp_path) {
            let _ = std::fs::remove_file(handle);
        }
        Ok(())
    }

    async fn create(
        &self,
        _req: Request,
        parent: &OsStr,
        name: &OsStr,
        _mode: u32,
        _flags: u32,
    ) -> FuseResult<ReplyCreated> {
        let filename = name.to_str().ok_or(FuseError::Invalid)?;
        let report = self
            .create_file_at_parent_path(Path::new(parent), filename)
            .map_err(fuse3::Errno::from)?;

        let fh = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let staging_id = self.client.staging_id();
        let temp_path =
            staging_root(self.client.state_root(), &staging_id).join(format!("{fh}.tmp"));
        std::fs::write(&temp_path, [])
            .map_err(|error| FuseError::Io(format!("failed to create write stage: {error}")))?;
        self.handles.lock().expect("fuse handles").insert(
            fh,
            OpenHandle {
                identifier: report.identifier,
                path: temp_path.clone(),
                writable: true,
                dirty: false,
                temp_path: Some(temp_path),
            },
        );

        Ok(ReplyCreated {
            ttl: ATTR_TTL,
            attr: attr_for_item(&report.item),
            generation: 0,
            fh,
            flags: 0,
        })
    }

    async fn mkdir(
        &self,
        _req: Request,
        parent: &OsStr,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
    ) -> FuseResult<ReplyEntry> {
        let parent_item = self.resolve_path(Path::new(parent))?;
        ensure_creatable_parent(&parent_item)?;
        let dirname = name.to_str().ok_or(FuseError::Invalid)?;
        let report = self
            .client
            .create_directory(&parent_item.identifier, dirname)?;
        self.cache_item_at(child_path(Path::new(parent), dirname), report.item.clone());

        Ok(ReplyEntry {
            ttl: ATTR_TTL,
            attr: attr_for_item(&report.item),
        })
    }

    async fn rename(
        &self,
        _req: Request,
        origin_parent: &OsStr,
        origin_name: &OsStr,
        parent: &OsStr,
        name: &OsStr,
    ) -> FuseResult<()> {
        let old_path = child_path(Path::new(origin_parent), &origin_name.to_string_lossy());
        let filename = name.to_str().ok_or(FuseError::Invalid)?;
        self.rename_path(&old_path, Path::new(parent), filename)
            .map_err(Into::into)
    }

    async fn unlink(&self, _req: Request, parent: &OsStr, name: &OsStr) -> FuseResult<()> {
        let path = child_path(Path::new(parent), &name.to_string_lossy());
        self.trash_path(&path, VirtualFsItemKind::File)?;
        Ok(())
    }

    async fn rmdir(&self, _req: Request, parent: &OsStr, name: &OsStr) -> FuseResult<()> {
        let path = child_path(Path::new(parent), &name.to_string_lossy());
        self.trash_path(&path, VirtualFsItemKind::Folder)?;
        Ok(())
    }

    async fn opendir(&self, _req: Request, path: &OsStr, _flags: u32) -> FuseResult<ReplyOpen> {
        let item = self.resolve_path(Path::new(path))?;
        if item.kind != VirtualFsItemKind::Folder {
            return Err(FuseError::NotDirectory.into());
        }
        Ok(ReplyOpen { fh: 0, flags: 0 })
    }

    async fn readdir<'a>(
        &'a self,
        _req: Request,
        path: &'a OsStr,
        _fh: u64,
        offset: i64,
    ) -> FuseResult<
        ReplyDirectory<impl futures_util::Stream<Item = FuseResult<DirectoryEntry>> + Send + 'a>,
    > {
        let item = self.resolve_path(Path::new(path))?;
        if item.kind != VirtualFsItemKind::Folder {
            return Err(FuseError::NotDirectory.into());
        }
        let parent_path = normalize_path(Path::new(path));
        let children = match self.client.children(&item.identifier) {
            Ok(children) => children,
            Err(error) if error.is_remote_missing() => {
                self.remove_cached_path(&parent_path);
                return Err(FuseError::NotFound.into());
            }
            Err(error) => return Err(error.into()),
        };
        let mut entries = Vec::new();
        entries.push(DirectoryEntry {
            kind: FileType::Directory,
            name: OsString::from("."),
            offset: 1,
        });
        entries.push(DirectoryEntry {
            kind: FileType::Directory,
            name: OsString::from(".."),
            offset: 2,
        });
        let mut cache = self.cache.lock().expect("fuse item cache");
        for child in directory_listing_items_for_parent(&parent_path, &item, children.children) {
            let offset = entries.len() as i64 + 1;
            let kind = file_type(&child);
            entries.push(DirectoryEntry {
                kind,
                name: OsString::from(&child.filename),
                offset,
            });
            cache.insert(child_path(&parent_path, &child.filename), child);
        }
        drop(cache);
        let entries = entries
            .into_iter()
            .filter(move |entry| entry.offset > offset)
            .map(Ok);
        Ok(ReplyDirectory {
            entries: stream::iter(entries),
        })
    }

    async fn readdirplus<'a>(
        &'a self,
        _req: Request,
        parent: &'a OsStr,
        _fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> FuseResult<
        ReplyDirectoryPlus<
            impl futures_util::Stream<Item = FuseResult<DirectoryEntryPlus>> + Send + 'a,
        >,
    > {
        let item = self.resolve_path(Path::new(parent))?;
        if item.kind != VirtualFsItemKind::Folder {
            return Err(FuseError::NotDirectory.into());
        }
        let parent_path = normalize_path(Path::new(parent));
        let parent_attr = attr_for_item(&item);
        let dotdot_attr = parent_path
            .parent()
            .and_then(|path| self.resolve_path(path).ok())
            .map(|item| attr_for_item(&item))
            .unwrap_or(parent_attr);
        let children = match self.client.children(&item.identifier) {
            Ok(children) => children,
            Err(error) if error.is_remote_missing() => {
                self.remove_cached_path(&parent_path);
                return Err(FuseError::NotFound.into());
            }
            Err(error) => return Err(error.into()),
        };
        let mut entries = Vec::new();
        entries.push(DirectoryEntryPlus {
            kind: FileType::Directory,
            name: OsString::from("."),
            offset: 1,
            attr: parent_attr,
            entry_ttl: ATTR_TTL,
            attr_ttl: ATTR_TTL,
        });
        entries.push(DirectoryEntryPlus {
            kind: FileType::Directory,
            name: OsString::from(".."),
            offset: 2,
            attr: dotdot_attr,
            entry_ttl: ATTR_TTL,
            attr_ttl: ATTR_TTL,
        });
        let mut cache = self.cache.lock().expect("fuse item cache");
        for child in directory_listing_items_for_parent(&parent_path, &item, children.children) {
            let offset = entries.len() as i64 + 1;
            let kind = file_type(&child);
            let attr = attr_for_item(&child);
            entries.push(DirectoryEntryPlus {
                kind,
                name: OsString::from(&child.filename),
                offset,
                attr,
                entry_ttl: ATTR_TTL,
                attr_ttl: ATTR_TTL,
            });
            cache.insert(child_path(&parent_path, &child.filename), child);
        }
        drop(cache);
        let entries = entries
            .into_iter()
            .filter(move |entry| entry.offset as u64 > offset)
            .map(Ok);
        Ok(ReplyDirectoryPlus {
            entries: stream::iter(entries),
        })
    }

    async fn access(&self, _req: Request, path: &OsStr, _mask: u32) -> FuseResult<()> {
        let item = self.resolve_path(Path::new(path))?;
        ensure_access_allowed(&item, _mask)?;
        Ok(())
    }

    async fn statfs(&self, _req: Request, _path: &OsStr) -> FuseResult<ReplyStatFs> {
        Ok(ReplyStatFs {
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
        })
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() || path == Path::new(ROOT_PATH) {
        return PathBuf::from(ROOT_PATH);
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(ROOT_PATH).join(path)
    }
}

fn child_path(parent: &Path, filename: &str) -> PathBuf {
    let mut path = normalize_path(parent);
    path.push(filename);
    path
}

fn open_is_writable(flags: u32) -> bool {
    let access = flags as i32 & libc::O_ACCMODE;
    access == libc::O_WRONLY || access == libc::O_RDWR || flags & libc::O_TRUNC as u32 != 0
}

fn ensure_writable_item(item: &VirtualFsItem) -> Result<(), FuseError> {
    if item.read_only {
        return Err(FuseError::ReadOnly);
    }
    if item.identifier == DIRECTORY_METADATA_IDENTIFIER {
        return Err(FuseError::ReadOnly);
    }
    if item.identifier.starts_with("schema:") {
        return Err(FuseError::ReadOnly);
    }
    if item
        .entity_kind
        .as_ref()
        .is_some_and(|kind| *kind != EntityKind::Page)
    {
        return Err(FuseError::ReadOnly);
    }
    Ok(())
}

fn ensure_creatable_parent(item: &VirtualFsItem) -> Result<(), FuseError> {
    if item.kind != VirtualFsItemKind::Folder {
        return Err(FuseError::NotDirectory);
    }
    if item.read_only {
        return Err(FuseError::ReadOnly);
    }
    Ok(())
}

fn ensure_access_allowed(item: &VirtualFsItem, mask: u32) -> Result<(), FuseError> {
    if mask & libc::W_OK as u32 == 0 {
        return Ok(());
    }
    if item.kind == VirtualFsItemKind::Folder {
        if item.read_only {
            return Err(FuseError::ReadOnly);
        }
        return Ok(());
    }
    ensure_writable_item(item)
}

fn file_type(item: &VirtualFsItem) -> FileType {
    match item.kind {
        VirtualFsItemKind::File => FileType::RegularFile,
        VirtualFsItemKind::Folder => FileType::Directory,
    }
}

fn attr_for_item(item: &VirtualFsItem) -> FileAttr {
    let attr_time = attr_time_for_item(item);
    let size = file_size_for_attr(item);
    FileAttr {
        size,
        blocks: size.div_ceil(512),
        atime: attr_time,
        mtime: attr_time,
        ctime: attr_time,
        #[cfg(target_os = "macos")]
        crtime: attr_time,
        kind: file_type(item),
        perm: match (&item.kind, item.read_only) {
            (VirtualFsItemKind::Folder, true) => 0o555,
            (VirtualFsItemKind::Folder, false) => 0o755,
            (VirtualFsItemKind::File, true) => 0o444,
            (VirtualFsItemKind::File, false) => 0o644,
        },
        nlink: if item.kind == VirtualFsItemKind::Folder {
            2
        } else {
            1
        },
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        #[cfg(target_os = "macos")]
        flags: 0,
        blksize: 4096,
    }
}

fn file_size_for_attr(item: &VirtualFsItem) -> u64 {
    if item.kind != VirtualFsItemKind::File {
        return 0;
    }
    item.materialized_path
        .as_ref()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.len())
        .or(item.byte_size)
        .unwrap_or(0)
}

fn attr_time_for_item(item: &VirtualFsItem) -> SystemTime {
    if item.kind == VirtualFsItemKind::File
        && let Some(modified) = item
            .materialized_path
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok())
            .and_then(|metadata| metadata.modified().ok())
    {
        return modified;
    }

    UNIX_EPOCH
}

#[cfg(test)]
mod tests {
    use super::*;
    use locality_store::InMemoryStateStore;
    #[test]
    fn folder_attrs_do_not_stat_materialized_path() {
        let path = std::env::temp_dir().join(format!(
            "locality-fuse-folder-attr-{}-{}",
            std::process::id(),
            unique_test_suffix()
        ));
        std::fs::write(&path, vec![0_u8; 900]).expect("write temp file");

        let item = test_item(VirtualFsItemKind::Folder, Some(path.clone()), None);
        let attr = attr_for_item(&item);

        let _ = std::fs::remove_file(path);
        assert_eq!(attr.size, 0);
        assert_eq!(attr.blocks, 0);
    }

    #[test]
    fn file_attrs_prefer_materialized_metadata_over_stale_reported_byte_size() {
        let path = std::env::temp_dir().join(format!(
            "locality-fuse-file-attr-{}-{}",
            std::process::id(),
            unique_test_suffix()
        ));
        std::fs::write(&path, vec![0_u8; 900]).expect("write temp file");

        let item = test_item(VirtualFsItemKind::File, Some(path.clone()), Some(1234));
        let attr = attr_for_item(&item);

        let _ = std::fs::remove_file(path);
        assert_eq!(attr.size, 900);
        assert_eq!(attr.blocks, 2);
    }

    #[test]
    fn file_attrs_use_reported_byte_size_without_materialized_path() {
        let item = test_item(VirtualFsItemKind::File, None, Some(1234));
        let attr = attr_for_item(&item);

        assert_eq!(attr.size, 1234);
        assert_eq!(attr.blocks, 3);
    }

    #[test]
    fn file_attrs_use_stable_materialized_modified_time() {
        let path = std::env::temp_dir().join(format!(
            "locality-fuse-file-attr-time-{}-{}",
            std::process::id(),
            unique_test_suffix()
        ));
        std::fs::write(&path, vec![0_u8; 16]).expect("write temp file");
        let modified = std::fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("modified");

        let item = test_item(VirtualFsItemKind::File, Some(path.clone()), None);
        let attr = attr_for_item(&item);

        let _ = std::fs::remove_file(path);
        assert_eq!(attr.mtime, modified);
        assert_eq!(attr.ctime, modified);
    }

    #[test]
    fn attrs_without_materialized_path_use_stable_epoch_time() {
        let item = test_item(VirtualFsItemKind::File, None, Some(10));
        let attr = attr_for_item(&item);

        assert_eq!(attr.mtime, UNIX_EPOCH);
        assert_eq!(attr.ctime, UNIX_EPOCH);
    }

    #[test]
    fn read_only_attrs_use_read_only_permissions_and_reject_writes() {
        let mut file = test_item(VirtualFsItemKind::File, None, Some(10));
        file.read_only = true;
        let file_attr = attr_for_item(&file);
        assert_eq!(file_attr.perm, 0o444);
        assert!(matches!(
            ensure_writable_item(&file),
            Err(FuseError::ReadOnly)
        ));

        let mut folder = test_item(VirtualFsItemKind::Folder, None, None);
        folder.read_only = true;
        let folder_attr = attr_for_item(&folder);
        assert_eq!(folder_attr.perm, 0o555);
        assert!(matches!(
            ensure_writable_item(&folder),
            Err(FuseError::ReadOnly)
        ));
    }

    #[test]
    fn read_only_parent_rejects_create_file_before_daemon_create() {
        let mut root = test_root_item();
        root.read_only = true;
        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: "notion-main".to_string(),
                root: root.clone(),
                children: BTreeMap::new(),
                created_files: Mutex::new(Vec::new()),
                created_item: None,
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::from([(PathBuf::from(ROOT_PATH), root)])),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        let error = fs
            .create_file_at_parent_path(Path::new(ROOT_PATH), "Draft.md")
            .expect_err("read-only parent rejects creates");

        assert!(matches!(error, FuseError::ReadOnly));
        assert!(
            fs.client
                .created_files
                .lock()
                .expect("created files")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn read_only_parent_rejects_mkdir_with_erofs() {
        let mut root = test_root_item();
        root.read_only = true;
        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: "notion-main".to_string(),
                root: root.clone(),
                children: BTreeMap::new(),
                created_files: Mutex::new(Vec::new()),
                created_item: None,
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::from([(PathBuf::from(ROOT_PATH), root)])),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        let error = fs
            .mkdir(
                Request::default(),
                OsStr::new(ROOT_PATH),
                OsStr::new("Draft"),
                0,
                0,
            )
            .await
            .expect_err("read-only parent rejects mkdir");

        assert_eq!(error, libc::EROFS.into());
    }

    #[tokio::test]
    async fn read_only_destination_parent_rejects_rename_before_daemon_rename() {
        let root = test_root_item();
        let source = test_named_item("draft-page", "Draft.md", VirtualFsItemKind::File);
        let mut read_only_parent =
            test_named_item("gmail-folder:inbox", "Inbox", VirtualFsItemKind::Folder);
        read_only_parent.read_only = true;
        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: "notion-main".to_string(),
                root: root.clone(),
                children: BTreeMap::new(),
                created_files: Mutex::new(Vec::new()),
                created_item: None,
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::from([
                (PathBuf::from(ROOT_PATH), root),
                (PathBuf::from("/Draft.md"), source),
                (PathBuf::from("/Inbox"), read_only_parent),
            ])),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        let error = fs
            .rename(
                Request::default(),
                OsStr::new(ROOT_PATH),
                OsStr::new("Draft.md"),
                OsStr::new("/Inbox"),
                OsStr::new("Moved.md"),
            )
            .await
            .expect_err("read-only destination parent rejects rename");

        assert_eq!(error, libc::EROFS.into());
        assert!(fs.client.renamed.lock().expect("renamed").is_empty());
    }

    #[tokio::test]
    async fn access_write_mask_rejects_read_only_item_with_erofs() {
        let root = test_root_item();
        let mut item = test_named_item("msg-inbox-1", "Inbox.md", VirtualFsItemKind::File);
        item.read_only = true;
        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: "notion-main".to_string(),
                root: root.clone(),
                children: BTreeMap::new(),
                created_files: Mutex::new(Vec::new()),
                created_item: None,
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::from([
                (PathBuf::from(ROOT_PATH), root),
                (PathBuf::from("/Inbox.md"), item),
            ])),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        let error = fs
            .access(
                Request::default(),
                OsStr::new("/Inbox.md"),
                libc::W_OK as u32,
            )
            .await
            .expect_err("read-only item rejects write access");

        assert_eq!(error, libc::EROFS.into());
    }

    #[tokio::test]
    async fn read_only_shared_mount_point_metadata_rejects_create_mkdir_and_write_access() {
        let mut store = InMemoryStateStore::new();
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-main",
        )
        .projection(ProjectionMode::LinuxFuse)
        .read_only(true);
        store.save_mount(mount).expect("save mount");
        let projection_children = localityd::virtual_projection::virtual_projection_root_children(
            &store,
            Path::new("/tmp/Locality"),
            ProjectionMode::LinuxFuse,
        )
        .expect("projection root children");
        let mount_item = projection_children
            .children
            .into_iter()
            .next()
            .expect("shared mount point item");
        assert!(mount_item.read_only);

        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: "notion-main".to_string(),
                root: shared_test_root_item(),
                children: BTreeMap::new(),
                created_files: Mutex::new(Vec::new()),
                created_item: None,
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::from([
                (PathBuf::from(ROOT_PATH), shared_test_root_item()),
                (PathBuf::from("/notion-main"), mount_item),
            ])),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        let create_error = fs
            .create_file_at_parent_path(Path::new("/notion-main"), "Draft.md")
            .expect_err("read-only shared mount rejects creates");
        assert!(matches!(create_error, FuseError::ReadOnly));
        assert!(
            fs.client
                .created_files
                .lock()
                .expect("created files")
                .is_empty()
        );

        let mkdir_error = fs
            .mkdir(
                Request::default(),
                OsStr::new("/notion-main"),
                OsStr::new("Draft"),
                0,
                0,
            )
            .await
            .expect_err("read-only shared mount rejects mkdir");
        assert_eq!(mkdir_error, libc::EROFS.into());

        let access_error = fs
            .access(
                Request::default(),
                OsStr::new("/notion-main"),
                libc::W_OK as u32,
            )
            .await
            .expect_err("read-only shared mount rejects write access");
        assert_eq!(access_error, libc::EROFS.into());
    }

    #[test]
    fn shared_root_readdir_items_include_directory_metadata() {
        let root = shared_test_root_item();
        let mount = test_named_item(
            &projection_root_identifier("notion-main"),
            "notion-main",
            VirtualFsItemKind::Folder,
        );

        let items = directory_listing_items_for_parent(Path::new(ROOT_PATH), &root, vec![mount]);

        assert_eq!(
            items
                .iter()
                .map(|item| item.filename.as_str())
                .collect::<Vec<_>>(),
            vec![".directory", "notion-main"]
        );
        assert_eq!(items[0].identifier, DIRECTORY_METADATA_IDENTIFIER);
    }

    #[test]
    fn directory_metadata_file_returns_exact_contents() {
        assert_eq!(
            virtual_file_contents(DIRECTORY_METADATA_IDENTIFIER),
            Some(b"[Desktop Entry]\nIcon=locality-mount-logo\n".as_slice())
        );

        let item = directory_metadata_item();
        assert_eq!(item.filename, ".directory");
        assert_eq!(item.path, ".directory");
        assert_eq!(
            item.byte_size,
            Some(DIRECTORY_METADATA_CONTENT.len() as u64)
        );
    }

    #[test]
    fn non_root_directories_do_not_get_directory_metadata() {
        let mount_root = test_root_item();
        let page_dir = test_named_item("children:page", "Page", VirtualFsItemKind::Folder);

        assert_eq!(
            directory_listing_items_for_parent(
                Path::new(ROOT_PATH),
                &mount_root,
                vec![page_dir.clone()],
            )
            .iter()
            .map(|item| item.filename.as_str())
            .collect::<Vec<_>>(),
            vec!["Page"]
        );
        assert_eq!(
            directory_listing_items_for_parent(Path::new("/Page"), &page_dir, Vec::new()),
            Vec::<VirtualFsItem>::new()
        );
    }

    #[test]
    fn shared_root_resolves_directory_metadata_without_daemon_child() {
        let root = shared_test_root_item();
        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: String::new(),
                root: root.clone(),
                children: BTreeMap::from([(ROOT_CONTAINER_IDENTIFIER.to_string(), Vec::new())]),
                created_files: Mutex::new(Vec::new()),
                created_item: None,
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::from([(PathBuf::from(ROOT_PATH), root)])),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        let item = fs
            .resolve_path(Path::new("/.directory"))
            .expect("resolve shared root metadata");

        assert_eq!(item.identifier, DIRECTORY_METADATA_IDENTIFIER);
        assert_eq!(item.filename, ".directory");
    }

    #[test]
    fn daemon_ping_result_reports_daemon_errors() {
        let error = daemon_ping_result(DaemonResponse::error(
            "not_ready",
            "daemon is still starting",
        ))
        .expect_err("daemon error should fail readiness");

        assert_eq!(error, "not_ready: daemon is still starting");
    }

    #[test]
    fn decode_response_maps_unsupported_operations_to_invalid() {
        let error = decode_response::<VirtualFsMutationReport>(DaemonResponse::error(
            "unsupported",
            "moving virtual filesystem files across parents is not supported yet",
        ))
        .expect_err("unsupported virtual filesystem operation");

        assert!(matches!(error, FuseError::Invalid));
    }

    #[test]
    fn parse_args_accepts_shared_root_without_mount_id() {
        let options = parse_args(vec![
            "--state-dir".to_string(),
            "/tmp/.loc".to_string(),
            "--mountpoint".to_string(),
            "/tmp/Locality".to_string(),
        ])
        .expect("parse shared root");

        assert_eq!(options.state_root, PathBuf::from("/tmp/.loc"));
        assert_eq!(options.mountpoint, PathBuf::from("/tmp/Locality"));
        assert_eq!(options.mount_id, None);
    }

    #[test]
    fn shared_root_staging_paths_are_distinct_per_mountpoint() {
        let state_root = PathBuf::from("/tmp/.loc");
        let first = DaemonClient {
            state_root: state_root.clone(),
            mount_id: None,
            mountpoint: PathBuf::from("/tmp/Locality"),
        };
        let second = DaemonClient {
            state_root: state_root.clone(),
            mount_id: None,
            mountpoint: PathBuf::from("/tmp/Other Locality"),
        };
        let per_mount = DaemonClient {
            state_root: state_root.clone(),
            mount_id: Some("notion-main".to_string()),
            mountpoint: PathBuf::from("/tmp/Locality/notion-main"),
        };

        assert_ne!(first.staging_id(), second.staging_id());
        assert_ne!(
            staging_root(first.state_root(), &first.staging_id()),
            staging_root(second.state_root(), &second.staging_id())
        );
        assert_eq!(per_mount.staging_id(), "notion-main");
        assert_eq!(
            staging_root(per_mount.state_root(), &per_mount.staging_id()),
            state_root.join("fuse-staging").join("notion-main")
        );
    }

    #[test]
    fn shared_root_staging_ids_are_bounded_and_collision_resistant() {
        let long_component = "x".repeat(300);
        let long_id = staging_id_for_mount(
            None,
            &PathBuf::from(format!("/tmp/{long_component}/Locality")),
        );
        let colon_id = staging_id_for_mount(None, Path::new("/tmp/a:b"));
        let question_id = staging_id_for_mount(None, Path::new("/tmp/a?b"));

        assert!(
            long_id.len() <= 80,
            "shared staging id should be bounded, got {} bytes: {long_id}",
            long_id.len()
        );
        assert_ne!(colon_id, question_id);
        assert_eq!(
            staging_id_for_mount(Some("notion-main"), Path::new("/tmp/Locality")),
            "notion-main"
        );
    }

    #[test]
    fn resolve_root_fetches_root_item_when_startup_cache_is_empty() {
        let root = test_root_item();
        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: "notion-main".to_string(),
                root: root.clone(),
                children: BTreeMap::new(),
                created_files: Mutex::new(Vec::new()),
                created_item: None,
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::new()),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        let item = fs.resolve_path(Path::new(ROOT_PATH)).expect("resolve root");

        assert_eq!(item, root);
        assert_eq!(item.identifier, "mount:notion-main");
        assert_eq!(
            fs.cache
                .lock()
                .expect("fuse item cache")
                .get(Path::new(ROOT_PATH)),
            Some(&root)
        );
    }

    #[test]
    fn root_level_create_uses_mount_point_identifier_as_parent() {
        let root = test_root_item();
        let created = test_named_item("local:draft", "Draft.md", VirtualFsItemKind::File);
        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: "notion-main".to_string(),
                root: root.clone(),
                children: BTreeMap::new(),
                created_files: Mutex::new(Vec::new()),
                created_item: Some(created.clone()),
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::from([(PathBuf::from(ROOT_PATH), root)])),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        let report = fs
            .create_file_at_parent_path(Path::new(ROOT_PATH), "Draft.md")
            .expect("create root file");

        assert_eq!(report.item, created);
        assert_eq!(
            fs.client
                .created_files
                .lock()
                .expect("created files")
                .as_slice(),
            &[("mount:notion-main".to_string(), "Draft.md".to_string())]
        );
    }

    #[test]
    fn resolve_path_refreshes_stale_local_cached_item() {
        let root = test_root_item();
        let parent = test_named_item("children:page-root", "Page", VirtualFsItemKind::Folder);
        let stale_dir = test_named_item("children:local:draft", "Draft", VirtualFsItemKind::Folder);
        let stale_page = test_named_item("local:draft", "page.md", VirtualFsItemKind::File);
        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: "notion-main".to_string(),
                root: root.clone(),
                children: BTreeMap::from([
                    (
                        "children:page-root".to_string(),
                        vec![test_named_item(
                            "children:page-draft",
                            "Draft",
                            VirtualFsItemKind::Folder,
                        )],
                    ),
                    (
                        "children:page-draft".to_string(),
                        vec![test_named_item(
                            "page-draft",
                            "page.md",
                            VirtualFsItemKind::File,
                        )],
                    ),
                ]),
                created_files: Mutex::new(Vec::new()),
                created_item: None,
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::from([
                (PathBuf::from(ROOT_PATH), root),
                (PathBuf::from("/Page"), parent),
                (PathBuf::from("/Page/Draft"), stale_dir),
                (PathBuf::from("/Page/Draft/page.md"), stale_page),
            ])),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        let item = fs
            .resolve_path(Path::new("/Page/Draft/page.md"))
            .expect("resolve refreshed item");

        assert_eq!(item.identifier, "page-draft");
        assert_eq!(
            fs.cache
                .lock()
                .expect("fuse item cache")
                .get(Path::new("/Page/Draft/page.md"))
                .map(|item| item.identifier.as_str()),
            Some("page-draft")
        );
    }

    #[test]
    fn resolve_path_removes_stale_cached_remote_folder_when_children_are_missing() {
        let root = test_root_item();
        let parent = test_named_item("children:page-root", "Page", VirtualFsItemKind::Folder);
        let stale_dir = test_named_item("children:page-stale", "Stale", VirtualFsItemKind::Folder);
        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: "notion-main".to_string(),
                root: root.clone(),
                children: BTreeMap::new(),
                created_files: Mutex::new(Vec::new()),
                created_item: None,
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::from([
                (PathBuf::from(ROOT_PATH), root),
                (PathBuf::from("/Page"), parent),
                (PathBuf::from("/Page/Stale"), stale_dir),
            ])),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        let error = fs
            .resolve_path(Path::new("/Page/Stale/page.md"))
            .expect_err("stale cached folder should disappear");

        assert!(matches!(error, FuseError::NotFound));
        assert!(
            !fs.cache
                .lock()
                .expect("fuse item cache")
                .contains_key(Path::new("/Page/Stale"))
        );
    }

    #[test]
    fn trash_path_accepts_folder_items_for_page_directory_delete() {
        let root = test_root_item();
        let page_dir = test_named_item("children:page-draft", "Draft", VirtualFsItemKind::Folder);
        let page_file = test_named_item("page-draft", "page.md", VirtualFsItemKind::File);
        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: "notion-main".to_string(),
                root: root.clone(),
                children: BTreeMap::new(),
                created_files: Mutex::new(Vec::new()),
                created_item: None,
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::from([
                (PathBuf::from(ROOT_PATH), root),
                (PathBuf::from("/Draft"), page_dir),
                (PathBuf::from("/Draft/page.md"), page_file),
            ])),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        fs.trash_path(Path::new("/Draft"), VirtualFsItemKind::Folder)
            .expect("trash folder");

        assert_eq!(
            fs.client.trashed.lock().expect("trashed").as_slice(),
            &["children:page-draft".to_string()]
        );
        assert!(
            !fs.cache
                .lock()
                .expect("fuse item cache")
                .contains_key(Path::new("/Draft"))
        );
        assert!(
            !fs.cache
                .lock()
                .expect("fuse item cache")
                .contains_key(Path::new("/Draft/page.md"))
        );
    }

    #[test]
    fn trash_path_treats_stale_pending_page_directory_as_already_removed() {
        let root = test_root_item();
        let stale_dir = test_named_item("children:local:draft", "Draft", VirtualFsItemKind::Folder);
        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: "notion-main".to_string(),
                root: root.clone(),
                children: BTreeMap::from([("mount:notion-main".to_string(), Vec::new())]),
                created_files: Mutex::new(Vec::new()),
                created_item: None,
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::from([
                (PathBuf::from(ROOT_PATH), root),
                (PathBuf::from("/Draft"), stale_dir),
            ])),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        fs.trash_path(Path::new("/Draft"), VirtualFsItemKind::Folder)
            .expect("stale pending folder was already removed");

        assert!(fs.client.trashed.lock().expect("trashed").is_empty());
        assert!(
            !fs.cache
                .lock()
                .expect("fuse item cache")
                .contains_key(Path::new("/Draft"))
        );
    }

    #[test]
    fn rename_path_accepts_folder_items_for_page_directory_rename() {
        let root = test_root_item();
        let page_dir = test_named_item("children:page-draft", "Draft", VirtualFsItemKind::Folder);
        let page_file = test_named_item("page-draft", "page.md", VirtualFsItemKind::File);
        let fs = AgentFuse {
            client: FakeClient {
                state_root: std::env::temp_dir(),
                mount_id: "notion-main".to_string(),
                root: root.clone(),
                children: BTreeMap::new(),
                created_files: Mutex::new(Vec::new()),
                created_item: None,
                renamed: Mutex::new(Vec::new()),
                trashed: Mutex::new(Vec::new()),
            },
            cache: Mutex::new(BTreeMap::from([
                (PathBuf::from(ROOT_PATH), root),
                (PathBuf::from("/Draft"), page_dir),
                (PathBuf::from("/Draft/page.md"), page_file),
            ])),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        };

        fs.rename_path(Path::new("/Draft"), Path::new(ROOT_PATH), "Published")
            .expect("rename folder");

        assert_eq!(
            fs.client.renamed.lock().expect("renamed").as_slice(),
            &[(
                "children:page-draft".to_string(),
                "mount:notion-main".to_string(),
                "Published".to_string()
            )]
        );
        assert!(
            !fs.cache
                .lock()
                .expect("fuse item cache")
                .contains_key(Path::new("/Draft"))
        );
        assert!(
            !fs.cache
                .lock()
                .expect("fuse item cache")
                .contains_key(Path::new("/Draft/page.md"))
        );
        assert!(
            fs.cache
                .lock()
                .expect("fuse item cache")
                .contains_key(Path::new("/Published"))
        );
    }

    fn test_item(
        kind: VirtualFsItemKind,
        materialized_path: Option<PathBuf>,
        byte_size: Option<u64>,
    ) -> VirtualFsItem {
        VirtualFsItem {
            identifier: "item".to_string(),
            parent_identifier: Some(ROOT_CONTAINER_IDENTIFIER.to_string()),
            filename: "Item".to_string(),
            kind,
            read_only: false,
            entity_kind: None,
            remote_id: None,
            path: "Item".to_string(),
            hydration: None,
            content_type: "public.data".to_string(),
            remote_edited_at: None,
            materialized_path: materialized_path.map(|path| path.display().to_string()),
            byte_size,
        }
    }

    fn test_named_item(identifier: &str, filename: &str, kind: VirtualFsItemKind) -> VirtualFsItem {
        VirtualFsItem {
            identifier: identifier.to_string(),
            parent_identifier: Some(ROOT_CONTAINER_IDENTIFIER.to_string()),
            filename: filename.to_string(),
            kind,
            read_only: false,
            entity_kind: None,
            remote_id: None,
            path: filename.to_string(),
            hydration: None,
            content_type: "public.data".to_string(),
            remote_edited_at: None,
            materialized_path: None,
            byte_size: None,
        }
    }

    fn test_root_item() -> VirtualFsItem {
        VirtualFsItem {
            identifier: "mount:notion-main".to_string(),
            parent_identifier: None,
            filename: String::new(),
            kind: VirtualFsItemKind::Folder,
            read_only: false,
            entity_kind: None,
            remote_id: None,
            path: String::new(),
            hydration: None,
            content_type: "public.folder".to_string(),
            remote_edited_at: None,
            materialized_path: None,
            byte_size: None,
        }
    }

    fn shared_test_root_item() -> VirtualFsItem {
        VirtualFsItem {
            identifier: ROOT_CONTAINER_IDENTIFIER.to_string(),
            parent_identifier: None,
            filename: String::new(),
            kind: VirtualFsItemKind::Folder,
            read_only: false,
            entity_kind: None,
            remote_id: None,
            path: String::new(),
            hydration: None,
            content_type: "public.folder".to_string(),
            remote_edited_at: None,
            materialized_path: None,
            byte_size: None,
        }
    }

    struct FakeClient {
        state_root: PathBuf,
        mount_id: String,
        root: VirtualFsItem,
        children: BTreeMap<String, Vec<VirtualFsItem>>,
        created_files: Mutex<Vec<(String, String)>>,
        created_item: Option<VirtualFsItem>,
        renamed: Mutex<Vec<(String, String, String)>>,
        trashed: Mutex<Vec<String>>,
    }

    impl VirtualFsClient for FakeClient {
        fn state_root(&self) -> &Path {
            &self.state_root
        }

        fn staging_id(&self) -> String {
            self.mount_id.clone()
        }

        fn root_identifier(&self) -> String {
            projection_root_identifier(&self.mount_id)
        }

        fn item(&self, identifier: &str) -> Result<VirtualFsItemReport, FuseError> {
            if identifier != projection_root_identifier(&self.mount_id) {
                return Err(FuseError::Daemon(format!("missing item {identifier}")));
            }
            Ok(VirtualFsItemReport {
                mount_id: self.mount_id.clone(),
                item: self.root.clone(),
            })
        }

        fn children(
            &self,
            container_identifier: &str,
        ) -> Result<VirtualFsChildrenReport, FuseError> {
            let children = self.children.get(container_identifier).ok_or_else(|| {
                FuseError::Daemon(format!(
                    "invalid_state: virtual filesystem item `{container_identifier}` is not present in daemon state"
                ))
            })?;
            Ok(VirtualFsChildrenReport {
                mount_id: self.mount_id.clone(),
                container_identifier: container_identifier.to_string(),
                children: children.clone(),
            })
        }

        fn materialize(&self, _identifier: &str) -> Result<VirtualFsMaterializeReport, FuseError> {
            Err(FuseError::Daemon(
                "unexpected materialize request".to_string(),
            ))
        }

        fn commit_write(
            &self,
            _identifier: &str,
            _bytes: Vec<u8>,
        ) -> Result<VirtualFsWriteReport, FuseError> {
            Err(FuseError::Daemon("unexpected commit request".to_string()))
        }

        fn create_file(
            &self,
            parent_identifier: &str,
            filename: &str,
        ) -> Result<VirtualFsMutationReport, FuseError> {
            self.created_files
                .lock()
                .expect("created files")
                .push((parent_identifier.to_string(), filename.to_string()));
            let item = self.created_item.clone().unwrap_or_else(|| {
                test_named_item("local:created", filename, VirtualFsItemKind::File)
            });
            Ok(VirtualFsMutationReport {
                mount_id: self.mount_id.clone(),
                identifier: item.identifier.clone(),
                path: filename.to_string(),
                item,
            })
        }

        fn create_directory(
            &self,
            _parent_identifier: &str,
            _dirname: &str,
        ) -> Result<VirtualFsMutationReport, FuseError> {
            Err(FuseError::Daemon(
                "unexpected create directory request".to_string(),
            ))
        }

        fn rename(
            &self,
            identifier: &str,
            new_parent_identifier: &str,
            new_filename: &str,
        ) -> Result<VirtualFsMutationReport, FuseError> {
            self.renamed.lock().expect("renamed").push((
                identifier.to_string(),
                new_parent_identifier.to_string(),
                new_filename.to_string(),
            ));
            let kind = if identifier.starts_with("children:") {
                VirtualFsItemKind::Folder
            } else {
                VirtualFsItemKind::File
            };
            Ok(VirtualFsMutationReport {
                mount_id: self.mount_id.clone(),
                identifier: identifier.to_string(),
                path: new_filename.to_string(),
                item: test_named_item(identifier, new_filename, kind),
            })
        }

        fn trash(&self, _identifier: &str) -> Result<VirtualFsMutationReport, FuseError> {
            self.trashed
                .lock()
                .expect("trashed")
                .push(_identifier.to_string());
            Ok(VirtualFsMutationReport {
                mount_id: self.mount_id.clone(),
                identifier: _identifier.to_string(),
                path: _identifier.to_string(),
                item: test_named_item(_identifier, _identifier, VirtualFsItemKind::Folder),
            })
        }
    }

    fn unique_test_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }
}
