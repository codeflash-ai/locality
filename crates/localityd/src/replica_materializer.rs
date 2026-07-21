//! Bounded, staged materialization of read-only backend replica archives.
//!
//! The materialized filesystem tree is the read representation. This module
//! intentionally has no repository dependency and creates no entity, shadow,
//! or per-file SQLite state.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
use std::fmt::{Display, Formatter};
use std::fs;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use rustix::fd::{AsFd, OwnedFd};
#[cfg(all(
    unix,
    any(target_vendor = "apple", target_os = "linux", target_os = "android")
))]
use rustix::fs::RenameFlags;
#[cfg(unix)]
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat};

use locality_core::portable::LogicalPath;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::remote_truth::{ReplicaArchive, ReplicaArchiveEncoding};

const TAR_BLOCK_BYTES: usize = 512;
const READ_ONLY_FILE_MODE: u32 = 0o444;
const READ_ONLY_DIRECTORY_MODE: u32 = 0o555;

/// Resource bounds applied before and during extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicaMaterializationLimits {
    pub max_entries: u64,
    pub max_file_bytes: u64,
    pub max_decoded_bytes: u64,
    pub max_disk_bytes: u64,
    /// Maximum Zstd window as a base-2 logarithm. The default is 8 MiB.
    pub max_zstd_window_log: u32,
}

impl Default for ReplicaMaterializationLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_file_bytes: 256 * 1024 * 1024,
            max_decoded_bytes: 4 * 1024 * 1024 * 1024,
            max_disk_bytes: 2 * 1024 * 1024 * 1024,
            max_zstd_window_log: 23,
        }
    }
}

/// Constant-size receipt for one successfully published tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReplicaMaterializationSummary {
    pub entries: u64,
    pub files: u64,
    pub directories: u64,
    pub materialized_bytes: u64,
    pub decoded_bytes: u64,
}

/// Exact decoded-tar receipt required before a staged tree may be published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpectedReplicaMaterializationReceipt {
    pub decoded_tar_sha256: [u8; 32],
    pub decoded_bytes: u64,
    pub entries: u64,
}

#[derive(Debug)]
pub enum ReplicaMaterializationError {
    InvalidDestination,
    DestinationParentMissing(PathBuf),
    DestinationExists(PathBuf),
    Staging(io::Error),
    Decode(String),
    MalformedTar(String),
    MissingTarEndMarker,
    TrailingTarData,
    TrailingZstdData,
    EntryLimit {
        limit: u64,
    },
    FileLimit {
        path: String,
        size: u64,
        limit: u64,
    },
    DecodedLimit {
        limit: u64,
    },
    DiskLimit {
        size: u64,
        limit: u64,
    },
    ReceiptDigestMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },
    ReceiptDecodedBytesMismatch {
        expected: u64,
        actual: u64,
    },
    ReceiptEntryCountMismatch {
        expected: u64,
        actual: u64,
    },
    NonUtf8Path,
    InvalidPath {
        path: String,
        reason: String,
    },
    UnsupportedEntryType {
        path: String,
    },
    LinkMetadata {
        path: String,
    },
    InvalidFileMode {
        path: String,
        mode: u32,
    },
    InvalidDirectoryMode {
        path: String,
        mode: u32,
    },
    NonEmptyDirectory {
        path: String,
    },
    DuplicatePath {
        path: String,
    },
    UnicodeCollision {
        first: String,
        second: String,
    },
    CaseCollision {
        first: String,
        second: String,
    },
    PathTypeCollision {
        path: String,
    },
    Write {
        path: PathBuf,
        source: io::Error,
    },
    Publish(io::Error),
}

impl Display for ReplicaMaterializationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDestination => {
                formatter.write_str("replica destination must have a parent and file name")
            }
            Self::DestinationParentMissing(path) => write!(
                formatter,
                "replica destination parent does not exist: {}",
                path.display()
            ),
            Self::DestinationExists(path) => write!(
                formatter,
                "replica destination already exists: {}",
                path.display()
            ),
            Self::Staging(error) => write!(
                formatter,
                "failed to create replica staging directory: {error}"
            ),
            Self::Decode(message) => write!(formatter, "invalid Zstd replica stream: {message}"),
            Self::MalformedTar(message) => {
                write!(formatter, "invalid replica tar stream: {message}")
            }
            Self::MissingTarEndMarker => {
                formatter.write_str("invalid replica tar stream: missing two-block end marker")
            }
            Self::TrailingTarData => {
                formatter.write_str("invalid replica tar stream: trailing data after end marker")
            }
            Self::TrailingZstdData => {
                formatter.write_str("invalid Zstd replica stream: multiple frames or trailing data")
            }
            Self::EntryLimit { limit } => {
                write!(formatter, "replica entry limit exceeded: {limit}")
            }
            Self::FileLimit { path, size, limit } => write!(
                formatter,
                "replica file `{path}` is {size} bytes, exceeding limit {limit}"
            ),
            Self::DecodedLimit { limit } => {
                write!(formatter, "replica decoded-byte limit exceeded: {limit}")
            }
            Self::DiskLimit { size, limit } => write!(
                formatter,
                "replica materialized bytes {size} exceed disk limit {limit}"
            ),
            Self::ReceiptDigestMismatch { expected, actual } => {
                formatter.write_str("replica decoded tar digest mismatch: expected sha256:")?;
                write_sha256(formatter, expected)?;
                formatter.write_str(", actual sha256:")?;
                write_sha256(formatter, actual)
            }
            Self::ReceiptDecodedBytesMismatch { expected, actual } => write!(
                formatter,
                "replica decoded-byte receipt mismatch: expected {expected}, actual {actual}"
            ),
            Self::ReceiptEntryCountMismatch { expected, actual } => write!(
                formatter,
                "replica entry-count receipt mismatch: expected {expected}, actual {actual}"
            ),
            Self::NonUtf8Path => formatter.write_str("replica tar entry path is not valid UTF-8"),
            Self::InvalidPath { path, reason } => {
                write!(formatter, "invalid replica path `{path}`: {reason}")
            }
            Self::UnsupportedEntryType { path } => write!(
                formatter,
                "replica entry `{path}` is not a regular file or directory"
            ),
            Self::LinkMetadata { path } => {
                write!(formatter, "replica entry `{path}` contains link metadata")
            }
            Self::InvalidFileMode { path, mode } => write!(
                formatter,
                "replica file `{path}` has mode {mode:04o}; expected 0444"
            ),
            Self::InvalidDirectoryMode { path, mode } => write!(
                formatter,
                "replica directory `{path}` has mode {mode:04o}; expected 0555"
            ),
            Self::NonEmptyDirectory { path } => {
                write!(formatter, "replica directory `{path}` contains data")
            }
            Self::DuplicatePath { path } => {
                write!(formatter, "replica path is duplicated: `{path}`")
            }
            Self::UnicodeCollision { first, second } => write!(
                formatter,
                "replica paths collide after Unicode normalization: `{first}` and `{second}`"
            ),
            Self::CaseCollision { first, second } => write!(
                formatter,
                "replica paths collide by case: `{first}` and `{second}`"
            ),
            Self::PathTypeCollision { path } => write!(
                formatter,
                "replica path is used as both a file and directory: `{path}`"
            ),
            Self::Write { path, source } => write!(
                formatter,
                "failed to materialize replica path `{}`: {source}",
                path.display()
            ),
            Self::Publish(error) => write!(
                formatter,
                "failed to publish replica tree atomically: {error}"
            ),
        }
    }
}

impl std::error::Error for ReplicaMaterializationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Staging(error) | Self::Publish(error) => Some(error),
            Self::Write { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Validate, extract, and atomically publish one read-only replica archive.
///
/// `destination` must not already exist. Staging is created beside it so the
/// final rename stays on one filesystem. Any failure removes the staging tree
/// and leaves the destination absent.
pub fn materialize_replica_archive<Body: Read>(
    archive: ReplicaArchive<Body>,
    destination: &Path,
    limits: ReplicaMaterializationLimits,
) -> Result<ReplicaMaterializationSummary, ReplicaMaterializationError> {
    materialize_replica_archive_inner(archive, destination, limits, None)
}

/// Validate, extract, receipt-check, and atomically publish one replica archive.
///
/// The SHA-256 identity is computed over the decoded tar bytes for both identity
/// and single-frame Zstd transports. A receipt mismatch removes staging and
/// leaves `destination` absent.
pub fn materialize_replica_archive_with_expected_receipt<Body: Read>(
    archive: ReplicaArchive<Body>,
    destination: &Path,
    limits: ReplicaMaterializationLimits,
    expected: ExpectedReplicaMaterializationReceipt,
) -> Result<ReplicaMaterializationSummary, ReplicaMaterializationError> {
    materialize_replica_archive_inner(archive, destination, limits, Some(expected))
}

fn materialize_replica_archive_inner<Body: Read>(
    archive: ReplicaArchive<Body>,
    destination: &Path,
    limits: ReplicaMaterializationLimits,
    expected: Option<ExpectedReplicaMaterializationReceipt>,
) -> Result<ReplicaMaterializationSummary, ReplicaMaterializationError> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(ReplicaMaterializationError::InvalidDestination)?;
    if destination.file_name().is_none() {
        return Err(ReplicaMaterializationError::InvalidDestination);
    }
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) | Err(_) => {
            return Err(ReplicaMaterializationError::DestinationParentMissing(
                parent.to_path_buf(),
            ));
        }
    }
    if fs::symlink_metadata(destination).is_ok() {
        return Err(ReplicaMaterializationError::DestinationExists(
            destination.to_path_buf(),
        ));
    }

    let mut staging = StagingDirectory::create(parent)?;
    let (summary, decoded_tar_sha256) = match archive.encoding {
        ReplicaArchiveEncoding::Identity => {
            let mut decoded = DecodedLimitReader::new(archive.body, limits.max_decoded_bytes);
            let result = extract_tar(&mut decoded, &staging, limits);
            let exceeded = decoded.exceeded();
            let decoded_bytes = decoded.consumed();
            let decoded_tar_sha256 = decoded.finish_sha256();
            if exceeded {
                return Err(ReplicaMaterializationError::DecodedLimit {
                    limit: limits.max_decoded_bytes,
                });
            }
            let mut summary = result?;
            summary.decoded_bytes = decoded_bytes;
            (summary, decoded_tar_sha256)
        }
        ReplicaArchiveEncoding::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::new(archive.body)
                .map_err(|error| ReplicaMaterializationError::Decode(error.to_string()))?;
            decoder
                .window_log_max(limits.max_zstd_window_log)
                .map_err(|error| ReplicaMaterializationError::Decode(error.to_string()))?;
            let mut decoder = decoder.single_frame();
            let (result, exceeded, decoded_bytes, decoded_tar_sha256) = {
                let mut decoded = DecodedLimitReader::new(&mut decoder, limits.max_decoded_bytes);
                let result = extract_tar(&mut decoded, &staging, limits);
                let exceeded = decoded.exceeded();
                let decoded_bytes = decoded.consumed();
                let decoded_tar_sha256 = decoded.finish_sha256();
                (result, exceeded, decoded_bytes, decoded_tar_sha256)
            };
            if exceeded {
                return Err(ReplicaMaterializationError::DecodedLimit {
                    limit: limits.max_decoded_bytes,
                });
            }
            let mut summary = result?;
            let mut compressed = decoder.finish();
            if read_one(&mut compressed)
                .map_err(|error| ReplicaMaterializationError::Decode(error.to_string()))?
                .is_some()
            {
                return Err(ReplicaMaterializationError::TrailingZstdData);
            }
            summary.decoded_bytes = decoded_bytes;
            (summary, decoded_tar_sha256)
        }
    };

    if let Some(expected) = expected {
        validate_receipt(expected, summary, decoded_tar_sha256)?;
    }

    make_tree_read_only(&staging).map_err(|source| ReplicaMaterializationError::Write {
        path: staging.path().to_path_buf(),
        source,
    })?;
    if fs::symlink_metadata(destination).is_ok() {
        return Err(ReplicaMaterializationError::DestinationExists(
            destination.to_path_buf(),
        ));
    }
    staging.publish(destination)?;
    Ok(summary)
}

fn validate_receipt(
    expected: ExpectedReplicaMaterializationReceipt,
    actual: ReplicaMaterializationSummary,
    actual_digest: [u8; 32],
) -> Result<(), ReplicaMaterializationError> {
    if actual_digest != expected.decoded_tar_sha256 {
        return Err(ReplicaMaterializationError::ReceiptDigestMismatch {
            expected: expected.decoded_tar_sha256,
            actual: actual_digest,
        });
    }
    if actual.decoded_bytes != expected.decoded_bytes {
        return Err(ReplicaMaterializationError::ReceiptDecodedBytesMismatch {
            expected: expected.decoded_bytes,
            actual: actual.decoded_bytes,
        });
    }
    if actual.entries != expected.entries {
        return Err(ReplicaMaterializationError::ReceiptEntryCountMismatch {
            expected: expected.entries,
            actual: actual.entries,
        });
    }
    Ok(())
}

fn write_sha256(formatter: &mut Formatter<'_>, digest: &[u8; 32]) -> std::fmt::Result {
    for byte in digest {
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

fn extract_tar<R: Read>(
    reader: &mut R,
    staging: &StagingDirectory,
    limits: ReplicaMaterializationLimits,
) -> Result<ReplicaMaterializationSummary, ReplicaMaterializationError> {
    let mut state = ExtractionState::default();
    {
        let mut archive = tar::Archive::new(reader.by_ref());
        let entries = archive
            .entries()
            .map_err(|error| ReplicaMaterializationError::MalformedTar(error.to_string()))?;
        for entry in entries {
            let mut entry = entry
                .map_err(|error| ReplicaMaterializationError::MalformedTar(error.to_string()))?;
            state.summary.entries = state.summary.entries.saturating_add(1);
            if state.summary.entries > limits.max_entries {
                return Err(ReplicaMaterializationError::EntryLimit {
                    limit: limits.max_entries,
                });
            }

            let entry_type = entry.header().entry_type();
            let is_directory = entry_type.is_dir();
            if !entry_type.is_file() && !is_directory {
                let path = display_path(entry.path_bytes().as_ref());
                return Err(ReplicaMaterializationError::UnsupportedEntryType { path });
            }
            if entry.header().link_name_bytes().is_some() {
                let path = display_path(entry.path_bytes().as_ref());
                return Err(ReplicaMaterializationError::LinkMetadata { path });
            }

            let path = validated_path(entry.path_bytes().as_ref(), is_directory)?;
            state.register_path(&path, is_directory)?;
            let mode = entry
                .header()
                .mode()
                .map_err(|error| ReplicaMaterializationError::MalformedTar(error.to_string()))?;
            if is_directory {
                if mode != READ_ONLY_DIRECTORY_MODE {
                    return Err(ReplicaMaterializationError::InvalidDirectoryMode { path, mode });
                }
                if entry.size() != 0 {
                    return Err(ReplicaMaterializationError::NonEmptyDirectory { path });
                }
                staging.create_directory(&path)?;
            } else {
                if mode != READ_ONLY_FILE_MODE {
                    return Err(ReplicaMaterializationError::InvalidFileMode { path, mode });
                }
                let size = entry.size();
                if size > limits.max_file_bytes {
                    return Err(ReplicaMaterializationError::FileLimit {
                        path,
                        size,
                        limit: limits.max_file_bytes,
                    });
                }
                let disk_size = state.summary.materialized_bytes.saturating_add(size);
                if disk_size > limits.max_disk_bytes {
                    return Err(ReplicaMaterializationError::DiskLimit {
                        size: disk_size,
                        limit: limits.max_disk_bytes,
                    });
                }
                staging.write_file(&path, &mut entry, size)?;
                state.summary.files += 1;
                state.summary.materialized_bytes = disk_size;
            }
        }
    }

    let mut end_block = [0_u8; TAR_BLOCK_BYTES];
    if reader.read_exact(&mut end_block).is_err() || end_block.iter().any(|byte| *byte != 0) {
        return Err(ReplicaMaterializationError::MissingTarEndMarker);
    }
    if read_one(reader)
        .map_err(|error| ReplicaMaterializationError::MalformedTar(error.to_string()))?
        .is_some()
    {
        return Err(ReplicaMaterializationError::TrailingTarData);
    }

    state.summary.directories = state.filesystem_directories.len() as u64;
    Ok(state.summary)
}

fn validated_path(
    raw_path: &[u8],
    is_directory: bool,
) -> Result<String, ReplicaMaterializationError> {
    let raw_path =
        std::str::from_utf8(raw_path).map_err(|_| ReplicaMaterializationError::NonUtf8Path)?;
    let path = if is_directory {
        raw_path.strip_suffix('/').unwrap_or(raw_path)
    } else {
        raw_path
    };
    let logical = LogicalPath::new(path.to_string()).map_err(|error| {
        ReplicaMaterializationError::InvalidPath {
            path: path.to_string(),
            reason: error.to_string(),
        }
    })?;
    Ok(logical.into_string())
}

fn display_path(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FilesystemKind {
    File,
    Directory,
}

#[derive(Default)]
struct ExtractionState {
    summary: ReplicaMaterializationSummary,
    archive_paths: BTreeSet<String>,
    filesystem_paths: BTreeMap<String, FilesystemKind>,
    filesystem_directories: BTreeSet<String>,
    unicode_paths: BTreeMap<String, String>,
    case_paths: BTreeMap<String, String>,
}

impl ExtractionState {
    fn register_path(
        &mut self,
        path: &str,
        is_directory: bool,
    ) -> Result<(), ReplicaMaterializationError> {
        if !self.archive_paths.insert(path.to_string()) {
            return Err(ReplicaMaterializationError::DuplicatePath {
                path: path.to_string(),
            });
        }

        let components = path.split('/').collect::<Vec<_>>();
        let mut prefix = String::new();
        for (index, component) in components.iter().enumerate() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            let is_leaf = index + 1 == components.len();
            let kind = if is_leaf && !is_directory {
                FilesystemKind::File
            } else {
                FilesystemKind::Directory
            };
            self.register_collision_key(&prefix)?;
            match self.filesystem_paths.get(&prefix) {
                Some(existing) if *existing != kind => {
                    return Err(ReplicaMaterializationError::PathTypeCollision { path: prefix });
                }
                Some(_) => {}
                None => {
                    self.filesystem_paths.insert(prefix.clone(), kind);
                    if kind == FilesystemKind::Directory {
                        self.filesystem_directories.insert(prefix.clone());
                    }
                }
            }
        }
        Ok(())
    }

    fn register_collision_key(&mut self, path: &str) -> Result<(), ReplicaMaterializationError> {
        let unicode_key = path.nfc().collect::<String>();
        if let Some(first) = self.unicode_paths.get(&unicode_key) {
            if first != path {
                return Err(ReplicaMaterializationError::UnicodeCollision {
                    first: first.clone(),
                    second: path.to_string(),
                });
            }
        } else {
            self.unicode_paths
                .insert(unicode_key.clone(), path.to_string());
        }

        let case_key = unicode_key.to_lowercase();
        if let Some(first) = self.case_paths.get(&case_key) {
            if first != path {
                return Err(ReplicaMaterializationError::CaseCollision {
                    first: first.clone(),
                    second: path.to_string(),
                });
            }
        } else {
            self.case_paths.insert(case_key, path.to_string());
        }
        Ok(())
    }
}

#[cfg(not(unix))]
fn write_file_at_path<R: Read>(
    path: &Path,
    reader: &mut R,
    expected_size: u64,
) -> Result<(), ReplicaMaterializationError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| ReplicaMaterializationError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    let written =
        io::copy(reader, &mut file).map_err(|source| ReplicaMaterializationError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    if written != expected_size {
        return Err(ReplicaMaterializationError::MalformedTar(format!(
            "entry `{}` ended after {written} of {expected_size} bytes",
            path.display()
        )));
    }
    file.flush()
        .map_err(|source| ReplicaMaterializationError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    set_file_read_only(path).map_err(|source| ReplicaMaterializationError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn make_tree_read_only(staging: &StagingDirectory) -> io::Result<()> {
    // macOS refuses to rename a directory whose own mode is 0555. Finalize
    // only children here; `publish` chmods the still-open root after rename.
    make_child_directories_read_only(&staging.root)
}

#[cfg(unix)]
fn make_child_directories_read_only(directory: &OwnedFd) -> io::Result<()> {
    let entries = Dir::read_from(directory)?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let metadata = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
        match FileType::from_raw_mode(metadata.st_mode) {
            FileType::Directory => {
                let child = open_directory_at(directory, name)?;
                make_child_directories_read_only(&child)?;
                rustix::fs::fchmod(&child, Mode::from_raw_mode(0o555))?;
            }
            FileType::RegularFile => {}
            _ => {
                return Err(io::Error::other(format!(
                    "staging tree contains a non-file, non-directory entry: {}",
                    name.to_string_lossy()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_tree_read_only(staging: &StagingDirectory) -> io::Result<()> {
    let root = staging.path();
    let mut directories = Vec::new();
    collect_directories(root, &mut directories)?;
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for directory in directories {
        // macOS refuses to rename a directory whose own mode is 0555. Keep
        // only the private staging root writable until the atomic rename;
        // `publish` applies its final mode before reporting success.
        if directory != root {
            set_directory_read_only(&directory)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn collect_directories(root: &Path, directories: &mut Vec<PathBuf>) -> io::Result<()> {
    directories.push(root.to_path_buf());
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            collect_directories(&entry.path(), directories)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_file_read_only(path: &Path) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_directory_read_only(path: &Path) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn make_tree_removable(root: &Path) {
    let Ok(metadata) = fs::symlink_metadata(root) else {
        return;
    };
    if metadata.is_dir() {
        let _ = make_directory_writable(root);
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                make_tree_removable(&entry.path());
            }
        }
    } else {
        let _ = make_file_writable(root);
    }
}

#[cfg(not(unix))]
fn make_directory_writable(path: &Path) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn make_file_writable(path: &Path) -> io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions)
}

#[cfg(unix)]
fn open_directory_at<Fd: AsFd, P: rustix::path::Arg>(
    directory: Fd,
    path: P,
) -> io::Result<OwnedFd> {
    rustix::fs::openat(
        directory,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(Into::into)
}

#[cfg(unix)]
fn remove_directory_contents(directory: &OwnedFd) {
    let _ = rustix::fs::fchmod(directory, Mode::from_raw_mode(0o700));
    let Ok(entries) = Dir::read_from(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let Ok(metadata) = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) else {
            continue;
        };
        if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory
            && let Ok(child) = open_directory_at(directory, name)
        {
            remove_directory_contents(&child);
            if rustix::fs::unlinkat(directory, name, AtFlags::REMOVEDIR).is_ok() {
                continue;
            }
        }
        // unlinkat without REMOVEDIR never follows symlinks and safely
        // removes regular files, links, devices, fifos, and sockets.
        let _ = rustix::fs::unlinkat(directory, name, AtFlags::empty());
    }
}

#[cfg(unix)]
fn same_file_identity(left: &Stat, right: &Stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

#[cfg(unix)]
fn named_entry_matches(directory: &OwnedFd, name: &OsStr, expected: &Stat) -> io::Result<bool> {
    let observed = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(same_file_identity(&observed, expected))
}

#[cfg(all(
    unix,
    any(target_vendor = "apple", target_os = "linux", target_os = "android")
))]
fn rename_directory_noreplace(
    directory: &OwnedFd,
    source: &OsStr,
    destination: &OsStr,
) -> io::Result<()> {
    rustix::fs::renameat_with(
        directory,
        source,
        directory,
        destination,
        RenameFlags::NOREPLACE,
    )
    .map_err(Into::into)
}

#[cfg(all(
    unix,
    not(any(target_vendor = "apple", target_os = "linux", target_os = "android"))
))]
fn rename_directory_noreplace(
    _directory: &OwnedFd,
    _source: &OsStr,
    _destination: &OsStr,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this Unix platform lacks atomic no-replace directory publication",
    ))
}

#[cfg(unix)]
fn unlink_named_entry(directory: &OwnedFd, name: &OsStr) {
    let Ok(metadata) = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) else {
        return;
    };
    let flags = if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    let _ = rustix::fs::unlinkat(directory, name, flags);
}

#[cfg(unix)]
fn remove_open_staging_directory(parent: &OwnedFd, root: &OwnedFd, hinted_name: &OsStr) {
    remove_directory_contents(root);
    let Ok(root_identity) = rustix::fs::fstat(root) else {
        return;
    };

    let matching_name = Dir::read_from(parent).ok().and_then(|entries| {
        entries.filter_map(Result::ok).find_map(|entry| {
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                return None;
            }
            let name = OsString::from_vec(name.to_bytes().to_vec());
            named_entry_matches(parent, &name, &root_identity)
                .ok()
                .filter(|matches| *matches)
                .map(|_| name)
        })
    });
    if let Some(name) = matching_name {
        let _ = rustix::fs::unlinkat(parent, &name, AtFlags::REMOVEDIR);
    }

    // A same-user substitution may leave a symlink or regular file at the
    // expected name. Remove that entry without following it. A non-empty
    // replacement directory is deliberately left untouched.
    unlink_named_entry(parent, hinted_name);
}

struct StagingDirectory {
    path: PathBuf,
    #[cfg(unix)]
    parent: OwnedFd,
    #[cfg(unix)]
    root: OwnedFd,
    #[cfg(unix)]
    name: OsString,
    published: bool,
}

impl StagingDirectory {
    fn create(parent: &Path) -> Result<Self, ReplicaMaterializationError> {
        // The pre-open metadata check alone is not enough: the path can be
        // replaced with a symlink before `open`. Keep the descriptor anchored
        // to the requested directory and let the identity checks below detect
        // replacements after this point.
        #[cfg(unix)]
        let parent_descriptor = rustix::fs::open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| ReplicaMaterializationError::Staging(error.into()))?;
        #[cfg(unix)]
        {
            let opened = rustix::fs::fstat(&parent_descriptor)
                .map_err(|error| ReplicaMaterializationError::Staging(error.into()))?;
            let named = rustix::fs::stat(parent)
                .map_err(|error| ReplicaMaterializationError::Staging(error.into()))?;
            if !same_file_identity(&opened, &named) {
                return Err(ReplicaMaterializationError::Staging(io::Error::other(
                    "replica destination parent changed while staging was created",
                )));
            }
        }
        for _ in 0..16 {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(|error| {
                ReplicaMaterializationError::Staging(io::Error::other(error.to_string()))
            })?;
            let suffix = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            #[cfg(unix)]
            let name = OsString::from(format!(".locality-stage-{suffix}"));
            #[cfg(unix)]
            let path = parent.join(&name);
            #[cfg(not(unix))]
            let path = parent.join(format!(".locality-stage-{suffix}"));
            #[cfg(unix)]
            let create_result =
                rustix::fs::mkdirat(&parent_descriptor, &name, Mode::from_raw_mode(0o700))
                    .map_err(io::Error::from);
            #[cfg(not(unix))]
            let create_result = fs::create_dir(&path);
            match create_result {
                Ok(()) => {
                    #[cfg(unix)]
                    let root = match rustix::fs::openat(
                        &parent_descriptor,
                        &name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    ) {
                        Ok(root) => root,
                        Err(error) => {
                            let _ =
                                rustix::fs::unlinkat(&parent_descriptor, &name, AtFlags::REMOVEDIR);
                            return Err(ReplicaMaterializationError::Staging(error.into()));
                        }
                    };
                    #[cfg(unix)]
                    if let Err(error) = rustix::fs::fchmod(&root, Mode::from_raw_mode(0o700)) {
                        let _ = rustix::fs::unlinkat(&parent_descriptor, &name, AtFlags::REMOVEDIR);
                        return Err(ReplicaMaterializationError::Staging(error.into()));
                    }
                    #[cfg(not(unix))]
                    make_directory_writable(&path).map_err(ReplicaMaterializationError::Staging)?;
                    return Ok(Self {
                        path,
                        #[cfg(unix)]
                        parent: parent_descriptor,
                        #[cfg(unix)]
                        root,
                        #[cfg(unix)]
                        name,
                        published: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(ReplicaMaterializationError::Staging(error)),
            }
        }
        Err(ReplicaMaterializationError::Staging(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique staging directory",
        )))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(unix)]
    fn open_or_create_directory(&self, logical_path: &str) -> io::Result<OwnedFd> {
        let mut current = None;
        for component in logical_path.split('/') {
            let parent = current
                .as_ref()
                .map(AsFd::as_fd)
                .unwrap_or_else(|| self.root.as_fd());
            let child = match rustix::fs::openat(
                parent,
                component,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(child) => child,
                Err(rustix::io::Errno::NOENT) => {
                    match rustix::fs::mkdirat(parent, component, Mode::from_raw_mode(0o700)) {
                        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                        Err(error) => return Err(error.into()),
                    }
                    open_directory_at(parent, component)?
                }
                Err(error) => return Err(error.into()),
            };
            current = Some(child);
        }
        current.ok_or_else(|| io::Error::other("empty logical directory path"))
    }

    #[cfg(unix)]
    fn create_directory(&self, logical_path: &str) -> Result<(), ReplicaMaterializationError> {
        self.open_or_create_directory(logical_path)
            .map(|_| ())
            .map_err(|source| ReplicaMaterializationError::Write {
                path: self.path.join(logical_path),
                source,
            })
    }

    #[cfg(not(unix))]
    fn create_directory(&self, logical_path: &str) -> Result<(), ReplicaMaterializationError> {
        let path = self.path.join(logical_path);
        fs::create_dir_all(&path)
            .map_err(|source| ReplicaMaterializationError::Write { path, source })
    }

    #[cfg(unix)]
    fn write_file<R: Read>(
        &self,
        logical_path: &str,
        reader: &mut R,
        expected_size: u64,
    ) -> Result<(), ReplicaMaterializationError> {
        let target = self.path.join(logical_path);
        let (parent_path, name) = logical_path
            .rsplit_once('/')
            .map_or((None, logical_path), |(parent, name)| (Some(parent), name));
        let parent = parent_path
            .map(|parent| self.open_or_create_directory(parent))
            .transpose()
            .map_err(|source| ReplicaMaterializationError::Write {
                path: target.clone(),
                source,
            })?;
        let directory = parent
            .as_ref()
            .map(AsFd::as_fd)
            .unwrap_or_else(|| self.root.as_fd());
        let descriptor = rustix::fs::openat(
            directory,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|source| ReplicaMaterializationError::Write {
            path: target.clone(),
            source: source.into(),
        })?;
        let mut file = fs::File::from(descriptor);
        let written =
            io::copy(reader, &mut file).map_err(|source| ReplicaMaterializationError::Write {
                path: target.clone(),
                source,
            })?;
        if written != expected_size {
            return Err(ReplicaMaterializationError::MalformedTar(format!(
                "entry `{}` ended after {written} of {expected_size} bytes",
                target.display()
            )));
        }
        file.flush()
            .map_err(|source| ReplicaMaterializationError::Write {
                path: target.clone(),
                source,
            })?;
        rustix::fs::fchmod(&file, Mode::from_raw_mode(0o444)).map_err(|source| {
            ReplicaMaterializationError::Write {
                path: target,
                source: source.into(),
            }
        })
    }

    #[cfg(not(unix))]
    fn write_file<R: Read>(
        &self,
        logical_path: &str,
        reader: &mut R,
        expected_size: u64,
    ) -> Result<(), ReplicaMaterializationError> {
        let path = self.path.join(logical_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ReplicaMaterializationError::Write {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        write_file_at_path(&path, reader, expected_size)
    }

    #[cfg(unix)]
    fn publish(&mut self, destination: &Path) -> Result<(), ReplicaMaterializationError> {
        let parent_path = self
            .path
            .parent()
            .ok_or(ReplicaMaterializationError::InvalidDestination)?
            .to_path_buf();
        if destination.parent() != Some(parent_path.as_path()) {
            return Err(ReplicaMaterializationError::InvalidDestination);
        }
        let destination_name = destination
            .file_name()
            .ok_or(ReplicaMaterializationError::InvalidDestination)?;
        let root_identity = rustix::fs::fstat(&self.root)
            .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        let parent_identity = rustix::fs::fstat(&self.parent)
            .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        let named_parent = rustix::fs::stat(&parent_path)
            .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        if !same_file_identity(&parent_identity, &named_parent)
            || !named_entry_matches(&self.parent, &self.name, &root_identity).unwrap_or(false)
        {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "replica staging root identity changed before publication",
            )));
        }

        if let Err(error) = rename_directory_noreplace(&self.parent, &self.name, destination_name) {
            if error.kind() == io::ErrorKind::AlreadyExists {
                return Err(ReplicaMaterializationError::DestinationExists(
                    destination.to_path_buf(),
                ));
            }
            return Err(ReplicaMaterializationError::Publish(error));
        }
        self.name = destination_name.to_os_string();
        self.path = destination.to_path_buf();

        if !named_entry_matches(&self.parent, &self.name, &root_identity).unwrap_or(false) {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "replica staging root identity changed during publication",
            )));
        }
        rustix::fs::fchmod(&self.root, Mode::from_raw_mode(0o555))
            .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        let named_parent = rustix::fs::stat(&parent_path)
            .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        if !same_file_identity(&parent_identity, &named_parent)
            || !named_entry_matches(&self.parent, &self.name, &root_identity).unwrap_or(false)
        {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "replica staging root identity changed while publication was finalized",
            )));
        }
        self.published = true;
        Ok(())
    }

    #[cfg(not(unix))]
    fn publish(&mut self, destination: &Path) -> Result<(), ReplicaMaterializationError> {
        match fs::rename(&self.path, destination) {
            Ok(()) => {
                if let Err(error) = set_directory_read_only(destination) {
                    let _ = fs::rename(destination, &self.path);
                    return Err(ReplicaMaterializationError::Publish(error));
                }
                self.published = true;
                Ok(())
            }
            Err(error) => Err(ReplicaMaterializationError::Publish(error)),
        }
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if !self.published {
            #[cfg(unix)]
            {
                remove_open_staging_directory(&self.parent, &self.root, &self.name);
            }
            #[cfg(not(unix))]
            {
                make_tree_removable(&self.path);
                let _ = fs::remove_dir_all(&self.path);
            }
        }
    }
}

struct DecodedLimitReader<R> {
    inner: R,
    limit: u64,
    consumed: u64,
    exceeded: bool,
    sha256: Sha256,
}

impl<R> DecodedLimitReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            consumed: 0,
            exceeded: false,
            sha256: Sha256::new(),
        }
    }

    fn consumed(&self) -> u64 {
        self.consumed
    }

    fn exceeded(&self) -> bool {
        self.exceeded
    }

    fn finish_sha256(self) -> [u8; 32] {
        self.sha256.finalize().into()
    }
}

impl<R: Read> Read for DecodedLimitReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.consumed == self.limit {
            let mut probe = [0_u8; 1];
            if self.inner.read(&mut probe)? == 0 {
                return Ok(0);
            }
            self.exceeded = true;
            return Err(io::Error::other("decoded-byte limit exceeded"));
        }
        let remaining = self.limit - self.consumed;
        let allowed = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.consumed += read as u64;
        self.sha256.update(&buffer[..read]);
        Ok(read)
    }
}

fn read_one(reader: &mut impl Read) -> io::Result<Option<u8>> {
    let mut byte = [0_u8; 1];
    match reader.read(&mut byte)? {
        0 => Ok(None),
        _ => Ok(Some(byte[0])),
    }
}
