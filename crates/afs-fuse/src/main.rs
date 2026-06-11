use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{Seek, SeekFrom, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use afs_core::model::EntityKind;
use afsd::ipc::{DaemonRequest, DaemonResponse, send_request};
use afsd::virtual_fs::{
    ROOT_CONTAINER_IDENTIFIER, VirtualFsChildrenReport, VirtualFsItem, VirtualFsItemKind,
    VirtualFsItemReport, VirtualFsMaterializeReport, VirtualFsWriteReport,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use fuse3::path::prelude::*;
use fuse3::{FileType, MountOptions, Result as FuseResult};
use futures_util::stream;
use serde::de::DeserializeOwned;

const ATTR_TTL: Duration = Duration::from_secs(1);
const ROOT_PATH: &str = "/";

#[derive(Clone, Debug)]
struct FuseOptions {
    mount_id: String,
    state_root: PathBuf,
    mountpoint: PathBuf,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("afs-fuse: {error}");
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
    std::fs::create_dir_all(staging_root(&options.state_root, &options.mount_id))
        .map_err(|error| format!("failed to create staging directory: {error}"))?;

    let fs = AgentFuse::new(DaemonClient {
        state_root: options.state_root.clone(),
        mount_id: options.mount_id.clone(),
    });
    let mut mount_options = MountOptions::default();
    mount_options.fs_name(format!("agentfs:{}", options.mount_id));
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
            "usage: afs-fuse --mount-id <id> --state-dir <path> --mountpoint <path>".to_string(),
        );
    }

    let mount_id = flag_value(&args, "--mount-id")
        .ok_or_else(|| "afs-fuse requires --mount-id <id>".to_string())?
        .to_string();
    let state_root = flag_value(&args, "--state-dir")
        .map(PathBuf::from)
        .unwrap_or_else(default_state_root);
    let mountpoint = flag_value(&args, "--mountpoint")
        .ok_or_else(|| "afs-fuse requires --mountpoint <path>".to_string())
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
    std::env::var("AFS_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|home| PathBuf::from(home).join(".afs")))
        .unwrap_or_else(|_| PathBuf::from(".afs"))
}

fn staging_root(state_root: &Path, mount_id: &str) -> PathBuf {
    state_root.join("fuse-staging").join(mount_id)
}

#[derive(Clone, Debug)]
struct DaemonClient {
    state_root: PathBuf,
    mount_id: String,
}

impl DaemonClient {
    fn item(&self, identifier: &str) -> Result<VirtualFsItemReport, FuseError> {
        self.request(&DaemonRequest::VirtualFsItem {
            mount_id: self.mount_id.clone(),
            identifier: identifier.to_string(),
        })
    }

    fn children(&self, container_identifier: &str) -> Result<VirtualFsChildrenReport, FuseError> {
        self.request(&DaemonRequest::VirtualFsChildren {
            mount_id: self.mount_id.clone(),
            container_identifier: container_identifier.to_string(),
        })
    }

    fn materialize(&self, identifier: &str) -> Result<VirtualFsMaterializeReport, FuseError> {
        self.request(&DaemonRequest::VirtualFsMaterialize {
            mount_id: self.mount_id.clone(),
            identifier: identifier.to_string(),
        })
    }

    fn commit_write(
        &self,
        identifier: &str,
        bytes: Vec<u8>,
    ) -> Result<VirtualFsWriteReport, FuseError> {
        self.request(&DaemonRequest::VirtualFsCommitWrite {
            mount_id: self.mount_id.clone(),
            identifier: identifier.to_string(),
            contents_base64: BASE64.encode(bytes),
        })
    }

    fn request<T>(&self, request: &DaemonRequest) -> Result<T, FuseError>
    where
        T: DeserializeOwned,
    {
        let response = send_request(&self.state_root, request)
            .map_err(|error| FuseError::Daemon(error.message().to_string()))?;
        decode_response(response)
    }
}

fn decode_response<T>(response: DaemonResponse) -> Result<T, FuseError>
where
    T: DeserializeOwned,
{
    if let Some(error) = response.error {
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

impl From<FuseError> for fuse3::Errno {
    fn from(error: FuseError) -> Self {
        match error {
            FuseError::NotFound => libc::ENOENT.into(),
            FuseError::NotFile => libc::EISDIR.into(),
            FuseError::NotDirectory => libc::ENOTDIR.into(),
            FuseError::ReadOnly => libc::EROFS.into(),
            FuseError::Invalid => libc::EINVAL.into(),
            FuseError::Daemon(message) | FuseError::Io(message) => {
                eprintln!("afs-fuse: {message}");
                libc::EIO.into()
            }
        }
    }
}

struct AgentFuse {
    client: DaemonClient,
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

impl AgentFuse {
    fn new(client: DaemonClient) -> Self {
        let mut cache = BTreeMap::new();
        if let Ok(report) = client.item(ROOT_CONTAINER_IDENTIFIER) {
            cache.insert(PathBuf::from(ROOT_PATH), report.item);
        }
        Self {
            client,
            cache: Mutex::new(cache),
            handles: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
        }
    }

    fn resolve_path(&self, path: &Path) -> Result<VirtualFsItem, FuseError> {
        let path = normalize_path(path);
        if let Some(item) = self
            .cache
            .lock()
            .expect("fuse item cache")
            .get(&path)
            .cloned()
        {
            return Ok(item);
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
        let children = self.client.children(&parent_item.identifier)?;
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
        let report = self.client.materialize(&item.identifier)?;
        self.update_cached_materialized_path(&report.identifier, &report.path);
        Ok(PathBuf::from(report.path))
    }

    fn update_cached_materialized_path(&self, identifier: &str, materialized_path: &str) {
        let mut cache = self.cache.lock().expect("fuse item cache");
        for item in cache.values_mut() {
            if item.identifier == identifier {
                item.materialized_path = Some(materialized_path.to_string());
            }
        }
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

impl PathFilesystem for AgentFuse {
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
        let path = path.ok_or(FuseError::NotFound)?;
        let item = self.resolve_path(Path::new(path))?;
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
        if item
            .entity_kind
            .as_ref()
            .is_some_and(|kind| *kind != EntityKind::Page)
        {
            return Err(FuseError::ReadOnly.into());
        }

        let writable = open_is_writable(flags);
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
            let temp_path = staging_root(&self.client.state_root, &self.client.mount_id)
                .join(format!("{fh}.tmp"));
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
            self.open_handle(fh)?.path
        } else {
            let path = path.ok_or(FuseError::NotFound)?;
            let item = self.resolve_path(Path::new(path))?;
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
        let children = self.client.children(&item.identifier)?;
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
        for child in children.children {
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
            .unwrap_or_else(|| parent_attr.clone());
        let children = self.client.children(&item.identifier)?;
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
        for child in children.children {
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
        self.resolve_path(Path::new(path))?;
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

fn file_type(item: &VirtualFsItem) -> FileType {
    match item.kind {
        VirtualFsItemKind::File => FileType::RegularFile,
        VirtualFsItemKind::Folder => FileType::Directory,
    }
}

fn attr_for_item(item: &VirtualFsItem) -> FileAttr {
    let now = SystemTime::now();
    let size = item
        .materialized_path
        .as_ref()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    FileAttr {
        size: if item.kind == VirtualFsItemKind::File {
            size
        } else {
            0
        },
        blocks: size.div_ceil(512),
        atime: now,
        mtime: now,
        ctime: now,
        kind: file_type(item),
        perm: if item.kind == VirtualFsItemKind::Folder {
            0o755
        } else {
            0o644
        },
        nlink: if item.kind == VirtualFsItemKind::Folder {
            2
        } else {
            1
        },
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 4096,
    }
}
