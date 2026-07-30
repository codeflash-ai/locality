//! Bounded, staged materialization of read-only backend replica archives.
//!
//! The materialized filesystem tree is the read representation. This module
//! intentionally has no repository dependency and creates no entity, shadow,
//! or per-file SQLite state.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(any(unix, windows))]
use std::ffi::{OsStr, OsString};
use std::fmt::{Display, Formatter};
use std::fs;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
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

use caseless::default_case_fold_str;
use locality_core::portable::LogicalPath;
use locality_protocol::{
    CanonicalControlOrderKey, CanonicalDirectoryOrderKey, CanonicalExportRecord,
    CanonicalFileOrderKey, DeliveredBodyDigestV2, ExportTerminalControlV2, ExportV2FilePaxMetadata,
    MAX_EXPORT_TERMINAL_CONTROL_BYTES, SealedExportOffer,
};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::remote_truth::{ReplicaArchive, ReplicaArchiveEncoding};
#[cfg(windows)]
#[path = "windows_workspace_fs.rs"]
mod windows_workspace_fs;
#[cfg(windows)]
use windows_workspace_fs::{WindowsDirectory, set_file_read_only as set_windows_file_read_only};

const TAR_BLOCK_BYTES: usize = 512;
const READ_ONLY_FILE_MODE: u32 = 0o444;
const READ_ONLY_DIRECTORY_MODE: u32 = 0o555;

/// Stable identity of one opened workspace generation. Publication journals
/// persist the equivalent values and pass them back into descriptor-anchored
/// exchange and cleanup operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceGenerationIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) inode_high: u64,
}

#[cfg(unix)]
fn workspace_identity_from_stat(stat: &Stat) -> WorkspaceGenerationIdentity {
    WorkspaceGenerationIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
        inode_high: 0,
    }
}

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
    ScopeExport(String),
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
            Self::ScopeExport(reason) => {
                write!(
                    formatter,
                    "invalid scope-authorized replica stream: {reason}"
                )
            }
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

/// Validate and atomically publish one scope-authorized v2 replica archive.
///
/// V2 directories and files are consumed in canonical order, followed by one
/// hidden `.loc/session.json` completion receipt. File PAX metadata is used
/// only while recomputing the inventory and delivered-body digests; it is not
/// written into the published tree.
pub fn materialize_scope_authorized_replica_archive<Body: Read>(
    archive: ReplicaArchive<Body>,
    destination: &Path,
    limits: ReplicaMaterializationLimits,
    offer: &SealedExportOffer,
) -> Result<ReplicaMaterializationSummary, ReplicaMaterializationError> {
    offer
        .validate()
        .map_err(|error| ReplicaMaterializationError::ScopeExport(error.to_string()))?;
    materialize_scope_authorized_replica_archive_inner(archive, destination, limits, offer)
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
    let staging_identity = staging.identity()?;
    staging.publish(destination, staging_identity)?;
    Ok(summary)
}

fn materialize_scope_authorized_replica_archive_inner<Body: Read>(
    archive: ReplicaArchive<Body>,
    destination: &Path,
    limits: ReplicaMaterializationLimits,
    offer: &SealedExportOffer,
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
    let mut summary = match archive.encoding {
        ReplicaArchiveEncoding::Identity => {
            let mut decoded = DecodedLimitReader::new(archive.body, limits.max_decoded_bytes);
            let result = extract_scope_authorized_tar(&mut decoded, &staging, limits, offer);
            let exceeded = decoded.exceeded();
            let decoded_bytes = decoded.consumed();
            if exceeded {
                return Err(ReplicaMaterializationError::DecodedLimit {
                    limit: limits.max_decoded_bytes,
                });
            }
            let mut summary = result?;
            summary.decoded_bytes = decoded_bytes;
            summary
        }
        ReplicaArchiveEncoding::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::new(archive.body)
                .map_err(|error| ReplicaMaterializationError::Decode(error.to_string()))?;
            decoder
                .window_log_max(limits.max_zstd_window_log)
                .map_err(|error| ReplicaMaterializationError::Decode(error.to_string()))?;
            let mut decoder = decoder.single_frame();
            let (result, exceeded, decoded_bytes) = {
                let mut decoded = DecodedLimitReader::new(&mut decoder, limits.max_decoded_bytes);
                let result = extract_scope_authorized_tar(&mut decoded, &staging, limits, offer);
                (result, decoded.exceeded(), decoded.consumed())
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
            summary
        }
    };

    make_tree_read_only(&staging).map_err(|source| ReplicaMaterializationError::Write {
        path: staging.path().to_path_buf(),
        source,
    })?;
    if fs::symlink_metadata(destination).is_ok() {
        return Err(ReplicaMaterializationError::DestinationExists(
            destination.to_path_buf(),
        ));
    }
    let staging_identity = staging.identity()?;
    staging.publish(destination, staging_identity)?;
    // The hidden control member is an archive entry, but not a materialized
    // file or byte. `extract_scope_authorized_tar` already counted it here.
    summary.entries = offer.archive_entry_count;
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

fn extract_scope_authorized_tar<R: Read>(
    reader: &mut R,
    staging: &StagingDirectory,
    limits: ReplicaMaterializationLimits,
    offer: &SealedExportOffer,
) -> Result<ReplicaMaterializationSummary, ReplicaMaterializationError> {
    let mut state = ExtractionState::default();
    let mut records = Vec::new();
    let mut terminal_control = None;
    let mut body_digest = DeliveredBodyDigestV2::new(offer.file_count);
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
            if terminal_control.is_some() {
                return Err(scope_export(
                    "the completion receipt is not the final member",
                ));
            }

            let entry_type = entry.header().entry_type();
            let is_directory = entry_type.is_dir();
            if !entry_type.is_file() && !is_directory {
                return Err(scope_export("an archive member has an unsupported type"));
            }
            if entry.header().link_name_bytes().is_some() {
                return Err(scope_export("an archive member contains link metadata"));
            }

            let raw_path = std::str::from_utf8(entry.path_bytes().as_ref())
                .map_err(|_| ReplicaMaterializationError::NonUtf8Path)?
                .to_string();
            let normalized_path = if is_directory {
                raw_path.strip_suffix('/').unwrap_or(&raw_path)
            } else {
                &raw_path
            }
            .to_string();
            let pax = locality_pax_fields(&mut entry)?;

            if normalized_path == locality_protocol::RESERVED_EXPORT_METADATA_PATH {
                if !entry_type.is_file() {
                    return Err(scope_export("the completion receipt is not a regular file"));
                }
                if !pax.is_empty() {
                    return Err(scope_export(
                        "the completion receipt has locality PAX metadata",
                    ));
                }
                let mode = entry.header().mode().map_err(|error| {
                    ReplicaMaterializationError::MalformedTar(error.to_string())
                })?;
                if mode != READ_ONLY_FILE_MODE {
                    return Err(scope_export("the completion receipt mode is not 0444"));
                }
                if entry.size() > MAX_EXPORT_TERMINAL_CONTROL_BYTES as u64 {
                    return Err(scope_export("the completion receipt is too large"));
                }
                let mut raw_control = Vec::with_capacity(entry.size() as usize);
                entry
                    .read_to_end(&mut raw_control)
                    .map_err(|_| scope_export("the completion receipt is malformed"))?;
                let parsed: ExportTerminalControlV2 = serde_json::from_slice(&raw_control)
                    .map_err(|_| scope_export("the completion receipt is malformed"))?;
                if serde_json::to_vec(&parsed).ok().as_deref() != Some(raw_control.as_slice()) {
                    return Err(scope_export("the completion receipt is not canonical JSON"));
                }
                records.push(CanonicalExportRecord::Control {
                    order_key: CanonicalControlOrderKey { ordinal: 0 },
                    member_path: locality_protocol::RESERVED_EXPORT_METADATA_PATH.to_string(),
                });
                terminal_control = Some(parsed);
                continue;
            }

            let path = validated_path(entry.path_bytes().as_ref(), is_directory)?;
            state.register_path(&path, is_directory)?;
            let mode = entry
                .header()
                .mode()
                .map_err(|error| ReplicaMaterializationError::MalformedTar(error.to_string()))?;
            if is_directory {
                if path.eq_ignore_ascii_case(".loc") {
                    return Err(scope_export(
                        "the reserved .loc directory header is forbidden",
                    ));
                }
                if !pax.is_empty() {
                    return Err(scope_export("a directory member has locality PAX metadata"));
                }
                if mode != READ_ONLY_DIRECTORY_MODE {
                    return Err(ReplicaMaterializationError::InvalidDirectoryMode { path, mode });
                }
                if entry.size() != 0 {
                    return Err(ReplicaMaterializationError::NonEmptyDirectory { path });
                }
                staging.create_directory(&path)?;
                records.push(CanonicalExportRecord::Directory {
                    order_key: CanonicalDirectoryOrderKey {
                        depth: path.split('/').count() as u32,
                        logical_path: LogicalPath::new(path).map_err(|error| {
                            scope_export(format!("a directory path is invalid: {error}"))
                        })?,
                    },
                });
                continue;
            }

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
            let metadata = ExportV2FilePaxMetadata::from_records(&pax)
                .ok_or_else(|| scope_export("a file has invalid locality PAX metadata"))?;
            let logical_path = LogicalPath::new(path.clone())
                .map_err(|error| scope_export(format!("a file path is invalid: {error}")))?;
            let parent_path = logical_path
                .as_str()
                .rsplit_once('/')
                .map(|(parent, _)| LogicalPath::new(parent))
                .transpose()
                .map_err(|_| scope_export("a file parent path is invalid"))?;
            body_digest
                .begin_file(&metadata.projection_id, size)
                .map_err(|error| scope_export(error.to_string()))?;
            let mut hashing = FileBodyHashReader::new(&mut entry, &mut body_digest);
            staging.write_file(&path, &mut hashing, size)?;
            let actual_content_sha256 = hashing.finish();
            body_digest
                .end_file()
                .map_err(|error| scope_export(error.to_string()))?;
            if actual_content_sha256 != metadata.content_sha256 {
                return Err(scope_export(
                    "a file body does not match its content digest",
                ));
            }
            records.push(CanonicalExportRecord::File {
                order_key: CanonicalFileOrderKey {
                    winning_scope_ordinal: metadata.winning_scope_ordinal,
                    parent_path,
                    logical_path,
                    projection_id: metadata.projection_id,
                },
                source_connection_id: metadata.source_connection_id,
                file_kind: metadata.file_kind,
                effective_actions: metadata.effective_actions,
                content_sha256: metadata.content_sha256,
                byte_length: size,
            });
            state.summary.files += 1;
            state.summary.materialized_bytes = disk_size;
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

    let terminal_control =
        terminal_control.ok_or_else(|| scope_export("the completion receipt is missing"))?;
    terminal_control
        .validate_against_inventory(offer, &records)
        .map_err(|error| scope_export(error.to_string()))?;
    let delivered_body_sha256 = body_digest
        .finish()
        .map_err(|error| scope_export(error.to_string()))?;
    if terminal_control.completion_receipt.delivered_body_sha256 != delivered_body_sha256 {
        return Err(scope_export(
            "the delivered-body digest does not match the completion receipt",
        ));
    }

    state.summary.directories = state.filesystem_directories.len() as u64;
    Ok(state.summary)
}

fn scope_export(reason: impl Into<String>) -> ReplicaMaterializationError {
    ReplicaMaterializationError::ScopeExport(reason.into())
}

struct FileBodyHashReader<'a, R> {
    inner: &'a mut R,
    content: Sha256,
    delivered: &'a mut DeliveredBodyDigestV2,
}

impl<'a, R> FileBodyHashReader<'a, R> {
    fn new(inner: &'a mut R, delivered: &'a mut DeliveredBodyDigestV2) -> Self {
        Self {
            inner,
            content: Sha256::new(),
            delivered,
        }
    }

    fn finish(self) -> String {
        format!("sha256:{:x}", self.content.finalize())
    }
}

impl<R: Read> Read for FileBodyHashReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(output)?;
        self.content.update(&output[..read]);
        self.delivered
            .update_file_chunk(&output[..read])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid delivered body"))?;
        Ok(read)
    }
}

fn locality_pax_fields<R: Read>(
    entry: &mut tar::Entry<'_, R>,
) -> Result<Vec<(String, String)>, ReplicaMaterializationError> {
    let mut fields = Vec::new();
    let Some(extensions) = entry
        .pax_extensions()
        .map_err(|_| scope_export("PAX metadata is malformed"))?
    else {
        return Ok(fields);
    };
    for extension in extensions {
        let extension = extension.map_err(|_| scope_export("PAX metadata is malformed"))?;
        let key_bytes = extension.key_bytes();
        if !key_bytes.starts_with(b"locality.") {
            continue;
        }
        let key = std::str::from_utf8(key_bytes)
            .map_err(|_| scope_export("a locality PAX key is not UTF-8"))?;
        let value = extension
            .value()
            .map_err(|_| scope_export("a locality PAX value is not UTF-8"))?;
        fields.push((key.to_string(), value.to_string()));
    }
    Ok(fields)
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
fn write_file_at_path<R: Read + ?Sized>(
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

#[cfg(unix)]
fn sync_directory_tree(directory: &OwnedFd) -> io::Result<()> {
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
                sync_directory_tree(&child)?;
            }
            FileType::RegularFile => {
                let file = rustix::fs::openat(
                    directory,
                    name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?;
                rustix::fs::fsync(&file)?;
            }
            _ => {
                return Err(io::Error::other(format!(
                    "staging tree contains a non-file, non-directory entry: {}",
                    name.to_string_lossy()
                )));
            }
        }
    }
    rustix::fs::fsync(directory).map_err(Into::into)
}

#[cfg(windows)]
fn make_tree_read_only(_staging: &StagingDirectory) -> io::Result<()> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
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

#[cfg(not(any(unix, windows)))]
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
fn sync_path_tree(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(io::Error::other(
                "staging tree contains a symbolic link or reparse point",
            ));
        }
        if file_type.is_dir() {
            sync_path_tree(&entry.path())?;
        } else if file_type.is_file() {
            fs::File::open(entry.path())?.sync_all()?;
        } else {
            return Err(io::Error::other(
                "staging tree contains a non-file, non-directory entry",
            ));
        }
    }
    sync_directory_if_supported(root)
}

#[cfg(not(unix))]
fn sync_directory_if_supported(path: &Path) -> io::Result<()> {
    match fs::File::open(path).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
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
fn remove_directory_contents(directory: &OwnedFd) -> io::Result<()> {
    rustix::fs::fchmod(directory, Mode::from_raw_mode(0o700))?;
    let entries = Dir::read_from(directory)?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let name_os = OsString::from_vec(name.to_bytes().to_vec());
        let metadata = match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) => metadata,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(error) => return Err(error.into()),
        };
        if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory
            && let Ok(child) = open_directory_at(directory, name)
        {
            let opened = rustix::fs::fstat(&child)?;
            if !same_file_identity(&metadata, &opened)
                || !named_entry_matches(directory, &name_os, &opened).unwrap_or(false)
            {
                return Err(io::Error::other(
                    "workspace cleanup entry changed before traversal",
                ));
            }
            remove_directory_contents(&child)?;
            if !named_entry_matches(directory, &name_os, &opened).unwrap_or(false) {
                return Err(io::Error::other(
                    "workspace cleanup entry changed before removal",
                ));
            }
            if rustix::fs::unlinkat(directory, name, AtFlags::REMOVEDIR).is_ok() {
                continue;
            }
        }
        // unlinkat without REMOVEDIR never follows symlinks and safely
        // removes regular files, links, devices, fifos, and sockets.
        let observed = match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(observed) => observed,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(error) => return Err(error.into()),
        };
        if !same_file_identity(&metadata, &observed) {
            return Err(io::Error::other(
                "workspace cleanup entry changed before removal",
            ));
        }
        rustix::fs::unlinkat(directory, name, AtFlags::empty())?;
    }
    Ok(())
}

#[cfg(unix)]
fn same_file_identity(left: &Stat, right: &Stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

#[cfg(unix)]
fn stat_matches_workspace_identity(stat: &Stat, expected: WorkspaceGenerationIdentity) -> bool {
    workspace_identity_from_stat(stat) == expected
}

#[cfg(unix)]
fn named_entry_matches(directory: &OwnedFd, name: &OsStr, expected: &Stat) -> io::Result<bool> {
    let observed = rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(same_file_identity(&observed, expected))
}

#[cfg(unix)]
fn open_anchored_parent(path: &Path) -> io::Result<(OwnedFd, OsString)> {
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| io::Error::other("workspace generation has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("workspace generation has no file name"))?
        .to_os_string();
    let parent = rustix::fs::open(
        parent_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let opened_parent = rustix::fs::fstat(&parent)?;
    let named_parent = rustix::fs::stat(parent_path)?;
    if !same_file_identity(&opened_parent, &named_parent) {
        return Err(io::Error::other(
            "workspace generation parent changed while it was opened",
        ));
    }
    Ok((parent, name))
}

#[cfg(unix)]
fn verify_anchored_parent(parent: &OwnedFd, parent_path: &Path) -> io::Result<Stat> {
    let opened = rustix::fs::fstat(parent)?;
    let named = rustix::fs::stat(parent_path)?;
    if !same_file_identity(&opened, &named) {
        return Err(io::Error::other(
            "workspace destination parent identity changed",
        ));
    }
    Ok(opened)
}

#[cfg(unix)]
fn preflight_existing_destination_spelling(
    parent: &OwnedFd,
    destination_name: &OsStr,
    expected: &Stat,
) -> io::Result<()> {
    let entries = Dir::read_from(parent)?;
    for entry in entries {
        let entry = entry?;
        let observed_name = entry.file_name();
        if observed_name.to_bytes() == b"." || observed_name.to_bytes() == b".." {
            continue;
        }
        let observed = match rustix::fs::statat(parent, observed_name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(observed) => observed,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(error) => return Err(error.into()),
        };
        if same_file_identity(&observed, expected) {
            if observed_name.to_bytes() != destination_name.as_bytes() {
                return Err(io::Error::other(
                    "workspace destination spelling changed on the filesystem",
                ));
            }
            return Ok(());
        }
    }
    Err(io::Error::other(
        "workspace destination identity is not bound to its requested spelling",
    ))
}

#[cfg(unix)]
fn filesystem_collision_key(name: &str) -> String {
    default_case_fold_str(name).nfc().collect()
}

#[cfg(unix)]
fn preflight_new_destination_spelling(
    parent: &OwnedFd,
    destination_name: &OsStr,
) -> io::Result<()> {
    let requested = destination_name
        .to_str()
        .ok_or_else(|| io::Error::other("workspace destination name is not UTF-8"))?;
    if requested.nfc().collect::<String>() != requested {
        return Err(io::Error::other(
            "workspace destination name is not canonical NFC",
        ));
    }
    let requested_key = filesystem_collision_key(requested);
    let entries = Dir::read_from(parent)?;
    for entry in entries {
        let entry = entry?;
        let observed_name = entry.file_name();
        if observed_name.to_bytes() == b"." || observed_name.to_bytes() == b".." {
            continue;
        }
        if observed_name.to_bytes() == destination_name.as_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "workspace destination already exists",
            ));
        }
        if let Ok(observed) = std::str::from_utf8(observed_name.to_bytes())
            && filesystem_collision_key(observed) == requested_key
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "workspace destination collides under filesystem spelling rules",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn workspace_generation_identity_if_exists(
    path: &Path,
) -> io::Result<Option<WorkspaceGenerationIdentity>> {
    let (parent, name) = open_anchored_parent(path)?;
    let stat = match rustix::fs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(io::Error::other(
            "workspace generation is not an ordinary directory",
        ));
    }
    let parent_stat = rustix::fs::fstat(&parent)?;
    if stat.st_dev != parent_stat.st_dev {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "workspace generation is on a different filesystem",
        ));
    }
    Ok(Some(workspace_identity_from_stat(&stat)))
}

#[cfg(windows)]
pub(crate) fn workspace_generation_identity_if_exists(
    path: &Path,
) -> io::Result<Option<WorkspaceGenerationIdentity>> {
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| io::Error::other("workspace generation has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("workspace generation has no file name"))?;
    let parent = WindowsDirectory::open_absolute(parent_path)?;
    match parent.open_directory(name) {
        Ok(directory) => directory.identity().map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn workspace_generation_identity_if_exists(
    path: &Path,
) -> io::Result<Option<WorkspaceGenerationIdentity>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            Ok(Some(WorkspaceGenerationIdentity {
                device: 0,
                inode: 0,
                inode_high: 0,
            }))
        }
        Ok(_) => Err(io::Error::other(
            "workspace generation is not an ordinary directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
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

#[cfg(target_os = "linux")]
fn workspace_root_is_mount_point(path: &Path) -> Result<bool, ReplicaMaterializationError> {
    let canonical = fs::canonicalize(path).map_err(ReplicaMaterializationError::Publish)?;
    let mountinfo =
        fs::read("/proc/self/mountinfo").map_err(ReplicaMaterializationError::Publish)?;
    for line in mountinfo.split(|byte| *byte == b'\n') {
        let Some(separator) = line.windows(3).position(|window| window == b" - ") else {
            continue;
        };
        let Some(encoded_mount_point) = line[..separator].split(|byte| *byte == b' ').nth(4) else {
            continue;
        };
        let mount_point = PathBuf::from(OsString::from_vec(decode_mountinfo_path(
            encoded_mount_point,
        )?));
        if mount_point == canonical {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_vendor = "apple")]
fn workspace_root_is_mount_point(path: &Path) -> Result<bool, ReplicaMaterializationError> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;

    let canonical = fs::canonicalize(path).map_err(ReplicaMaterializationError::Publish)?;
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        ReplicaMaterializationError::Publish(io::Error::other("workspace destination contains NUL"))
    })?;
    let mut stat = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `statfs` initializes the output on
    // success. The mount-name field is a fixed-size C character array.
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(ReplicaMaterializationError::Publish(
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: successful `statfs` initialized `stat` above.
    let stat = unsafe { stat.assume_init() };
    let mount_name_bytes = stat
        .f_mntonname
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    Ok(canonical.as_os_str().as_bytes() == mount_name_bytes)
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
fn workspace_root_is_mount_point(_path: &Path) -> Result<bool, ReplicaMaterializationError> {
    Ok(false)
}

#[cfg(target_os = "linux")]
fn decode_mountinfo_path(input: &[u8]) -> Result<Vec<u8>, ReplicaMaterializationError> {
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0;
    while cursor < input.len() {
        if input[cursor] != b'\\' {
            output.push(input[cursor]);
            cursor += 1;
            continue;
        }
        let digits = input.get(cursor + 1..cursor + 4).ok_or_else(|| {
            ReplicaMaterializationError::Publish(io::Error::other("malformed mountinfo escape"))
        })?;
        if !digits.iter().all(|digit| matches!(digit, b'0'..=b'7')) {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "malformed mountinfo escape",
            )));
        }
        let value = (digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + (digits[2] - b'0');
        output.push(value);
        cursor += 4;
    }
    Ok(output)
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
fn remove_open_staging_directory(parent: &OwnedFd, root: &OwnedFd, hinted_name: &OsStr) {
    let _ = remove_directory_contents(root);
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
    let _ = hinted_name;
}

pub(crate) fn remove_workspace_generation(
    path: &Path,
    expected: WorkspaceGenerationIdentity,
) -> io::Result<()> {
    #[cfg(not(unix))]
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| io::Error::other("workspace generation has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("workspace generation has no file name"))?;
    #[cfg(unix)]
    {
        let (parent, anchored_name) = open_anchored_parent(path)?;
        if anchored_name != name {
            return Err(io::Error::other("workspace cleanup name changed"));
        }
        let root = rustix::fs::openat(
            &parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let opened = rustix::fs::fstat(&root)?;
        if !stat_matches_workspace_identity(&opened, expected)
            || !named_entry_matches(&parent, name, &opened).unwrap_or(false)
        {
            return Err(io::Error::other(
                "workspace generation identity changed before cleanup",
            ));
        }
        remove_directory_contents(&root)?;
        if !named_entry_matches(&parent, name, &opened).unwrap_or(false) {
            return Err(io::Error::other(
                "workspace generation identity changed before removal",
            ));
        }
        rustix::fs::unlinkat(&parent, name, AtFlags::REMOVEDIR)?;
        rustix::fs::fsync(&parent)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        let parent = WindowsDirectory::open_absolute(parent_path)?;
        let root = parent.open_directory(name)?;
        if root.identity()? != expected {
            return Err(io::Error::other(
                "workspace generation identity changed before cleanup",
            ));
        }
        root.remove_contents(path)?;
        if parent.open_directory(name)?.identity()? != expected {
            return Err(io::Error::other(
                "workspace generation identity changed before removal",
            ));
        }
        root.mark_delete()?;
        parent.sync()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::other(
                "workspace generation is not an ordinary directory",
            ));
        }
        let _ = expected;
        make_tree_removable(path);
        fs::remove_dir_all(path)?;
        sync_directory_if_supported(parent_path)
    }
}

pub(crate) fn repair_workspace_generation(
    path: &Path,
    expected: WorkspaceGenerationIdentity,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        let (parent, name) = open_anchored_parent(path)?;
        let root = rustix::fs::openat(
            &parent,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let opened = rustix::fs::fstat(&root)?;
        if !stat_matches_workspace_identity(&opened, expected)
            || !named_entry_matches(&parent, &name, &opened).unwrap_or(false)
        {
            return Err(io::Error::other(
                "workspace generation identity changed before mode repair",
            ));
        }
        rustix::fs::fchmod(&root, Mode::from_raw_mode(0o555))?;
        rustix::fs::fsync(&root)?;
        if !named_entry_matches(&parent, &name, &opened).unwrap_or(false) {
            return Err(io::Error::other(
                "workspace generation identity changed while mode was repaired",
            ));
        }
        rustix::fs::fsync(&parent)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        let parent_path = path
            .parent()
            .ok_or_else(|| io::Error::other("workspace generation has no parent"))?;
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::other("workspace generation has no file name"))?;
        let parent = WindowsDirectory::open_absolute(parent_path)?;
        let root = parent.open_directory(name)?;
        if root.identity()? != expected {
            return Err(io::Error::other(
                "workspace generation identity changed before mode repair",
            ));
        }
        root.set_read_only()?;
        root.sync()?;
        if parent.open_directory(name)?.identity()? != expected {
            return Err(io::Error::other(
                "workspace generation identity changed while mode was repaired",
            ));
        }
        parent.sync()
    }
    #[cfg(not(any(unix, windows)))]
    {
        if workspace_generation_identity_if_exists(path)? != Some(expected) {
            return Err(io::Error::other(
                "workspace generation identity changed before mode repair",
            ));
        }
        set_directory_read_only(path)?;
        sync_directory_if_supported(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("workspace generation has no parent"))?;
        sync_directory_if_supported(parent)
    }
}

pub(crate) struct StagingDirectory {
    path: PathBuf,
    #[cfg(unix)]
    parent: OwnedFd,
    #[cfg(unix)]
    root: OwnedFd,
    #[cfg(unix)]
    name: OsString,
    #[cfg(windows)]
    parent: WindowsDirectory,
    #[cfg(windows)]
    root: WindowsDirectory,
    #[cfg(windows)]
    name: OsString,
    published: bool,
}

impl StagingDirectory {
    pub(crate) fn create(parent: &Path) -> Result<Self, ReplicaMaterializationError> {
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
        #[cfg(windows)]
        let parent_descriptor = WindowsDirectory::open_absolute(parent)
            .map_err(ReplicaMaterializationError::Staging)?;
        for _ in 0..16 {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(|error| {
                ReplicaMaterializationError::Staging(io::Error::other(error.to_string()))
            })?;
            let suffix = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            #[cfg(any(unix, windows))]
            let name = OsString::from(format!(".locality-stage-{suffix}"));
            #[cfg(any(unix, windows))]
            let path = parent.join(&name);
            #[cfg(not(any(unix, windows)))]
            let path = parent.join(format!(".locality-stage-{suffix}"));
            #[cfg(unix)]
            let create_result =
                rustix::fs::mkdirat(&parent_descriptor, &name, Mode::from_raw_mode(0o700))
                    .map(|()| None::<()>)
                    .map_err(io::Error::from);
            #[cfg(windows)]
            let create_result = parent_descriptor.create_directory(&name).map(Some);
            #[cfg(not(any(unix, windows)))]
            let create_result = fs::create_dir(&path).map(|()| None::<()>);
            match create_result {
                Ok(_created_root) => {
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
                    #[cfg(windows)]
                    let root = _created_root.expect("Windows create returns an opened root");
                    #[cfg(not(any(unix, windows)))]
                    make_directory_writable(&path).map_err(ReplicaMaterializationError::Staging)?;
                    return Ok(Self {
                        path,
                        #[cfg(unix)]
                        parent: parent_descriptor,
                        #[cfg(unix)]
                        root,
                        #[cfg(unix)]
                        name,
                        #[cfg(windows)]
                        parent: parent_descriptor,
                        #[cfg(windows)]
                        root,
                        #[cfg(windows)]
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

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn identity(
        &self,
    ) -> Result<WorkspaceGenerationIdentity, ReplicaMaterializationError> {
        #[cfg(unix)]
        {
            let root = rustix::fs::fstat(&self.root)
                .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
            Ok(workspace_identity_from_stat(&root))
        }
        #[cfg(windows)]
        {
            self.root
                .identity()
                .map_err(ReplicaMaterializationError::Publish)
        }
        #[cfg(not(any(unix, windows)))]
        {
            workspace_generation_identity_if_exists(&self.path)
                .map_err(ReplicaMaterializationError::Publish)?
                .ok_or_else(|| {
                    ReplicaMaterializationError::Publish(io::Error::other(
                        "workspace staging root disappeared",
                    ))
                })
        }
    }

    pub(crate) fn finalize_durable(&self) -> Result<(), ReplicaMaterializationError> {
        make_tree_read_only(self).map_err(|source| ReplicaMaterializationError::Write {
            path: self.path.clone(),
            source,
        })?;
        #[cfg(unix)]
        sync_directory_tree(&self.root).map_err(|source| ReplicaMaterializationError::Write {
            path: self.path.clone(),
            source,
        })?;
        #[cfg(windows)]
        self.root
            .sync()
            .map_err(|source| ReplicaMaterializationError::Write {
                path: self.path.clone(),
                source,
            })?;
        #[cfg(not(any(unix, windows)))]
        sync_path_tree(&self.path).map_err(|source| ReplicaMaterializationError::Write {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }

    pub(crate) fn sync_parent(&self) -> Result<(), ReplicaMaterializationError> {
        #[cfg(unix)]
        {
            rustix::fs::fsync(&self.parent)
                .map_err(io::Error::from)
                .map_err(ReplicaMaterializationError::Publish)
        }
        #[cfg(windows)]
        {
            self.parent
                .sync()
                .map_err(ReplicaMaterializationError::Publish)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let parent = self
                .path
                .parent()
                .ok_or(ReplicaMaterializationError::InvalidDestination)?;
            sync_directory_if_supported(parent).map_err(ReplicaMaterializationError::Publish)
        }
    }

    #[cfg(all(
        unix,
        any(target_vendor = "apple", target_os = "linux", target_os = "android")
    ))]
    pub(crate) fn exchange(
        &mut self,
        destination: &Path,
        expected_staging: WorkspaceGenerationIdentity,
        expected_destination: WorkspaceGenerationIdentity,
    ) -> Result<PathBuf, ReplicaMaterializationError> {
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
        let old_identity =
            rustix::fs::statat(&self.parent, destination_name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        if FileType::from_raw_mode(old_identity.st_mode) != FileType::Directory {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "workspace publication root is not an ordinary directory",
            )));
        }
        if !stat_matches_workspace_identity(&root_identity, expected_staging)
            || !stat_matches_workspace_identity(&old_identity, expected_destination)
        {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "workspace generation identity changed before exchange",
            )));
        }
        let parent_identity = rustix::fs::fstat(&self.parent)
            .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        if old_identity.st_dev != parent_identity.st_dev
            || workspace_root_is_mount_point(destination)?
        {
            return Err(ReplicaMaterializationError::Publish(io::Error::new(
                io::ErrorKind::Unsupported,
                "workspace publication root is a mount point or different filesystem",
            )));
        }
        let parent_identity = verify_anchored_parent(&self.parent, &parent_path)
            .map_err(ReplicaMaterializationError::Publish)?;
        let immediate_old =
            rustix::fs::statat(&self.parent, destination_name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        let immediate_root = rustix::fs::fstat(&self.root)
            .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        if !stat_matches_workspace_identity(&immediate_old, expected_destination)
            || !stat_matches_workspace_identity(&immediate_root, expected_staging)
            || immediate_old.st_dev != parent_identity.st_dev
            || !named_entry_matches(&self.parent, &self.name, &immediate_root).unwrap_or(false)
        {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "workspace generation identity changed immediately before exchange",
            )));
        }
        preflight_existing_destination_spelling(&self.parent, destination_name, &immediate_old)
            .map_err(ReplicaMaterializationError::Publish)?;
        if !named_entry_matches(&self.parent, &self.name, &root_identity).unwrap_or(false) {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "workspace staging root identity changed before exchange",
            )));
        }
        let swap_old =
            rustix::fs::statat(&self.parent, destination_name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        let swap_staging = rustix::fs::statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        if !stat_matches_workspace_identity(&swap_old, expected_destination)
            || !stat_matches_workspace_identity(&swap_staging, expected_staging)
        {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "workspace generation identity changed at exchange",
            )));
        }
        let old_path = self.path.clone();
        rustix::fs::renameat_with(
            &self.parent,
            &self.name,
            &self.parent,
            destination_name,
            RenameFlags::EXCHANGE,
        )
        .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        self.published = true;
        if !named_entry_matches(&self.parent, destination_name, &root_identity).unwrap_or(false)
            || !named_entry_matches(&self.parent, &self.name, &old_identity).unwrap_or(false)
        {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "workspace root identity changed during atomic exchange",
            )));
        }
        rustix::fs::fchmod(&self.root, Mode::from_raw_mode(0o555))
            .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        rustix::fs::fsync(&self.root)
            .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        self.path = destination.to_path_buf();
        self.sync_parent()?;
        Ok(old_path)
    }

    #[cfg(not(all(
        unix,
        any(target_vendor = "apple", target_os = "linux", target_os = "android")
    )))]
    pub(crate) fn exchange(
        &mut self,
        _destination: &Path,
        _expected_staging: WorkspaceGenerationIdentity,
        _expected_destination: WorkspaceGenerationIdentity,
    ) -> Result<PathBuf, ReplicaMaterializationError> {
        Err(ReplicaMaterializationError::Publish(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic workspace directory exchange is unavailable on this platform",
        )))
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
    pub(crate) fn create_directory(
        &self,
        logical_path: &str,
    ) -> Result<(), ReplicaMaterializationError> {
        self.open_or_create_directory(logical_path)
            .map(|_| ())
            .map_err(|source| ReplicaMaterializationError::Write {
                path: self.path.join(logical_path),
                source,
            })
    }

    #[cfg(windows)]
    fn open_or_create_windows_directory(&self, logical_path: &str) -> io::Result<WindowsDirectory> {
        let mut current = None;
        for component in logical_path.split('/') {
            let parent = current.as_ref().unwrap_or(&self.root);
            let child = parent.open_or_create_directory(OsStr::new(component))?;
            child.set_read_only()?;
            current = Some(child);
        }
        current.ok_or_else(|| io::Error::other("empty logical directory path"))
    }

    #[cfg(windows)]
    pub(crate) fn create_directory(
        &self,
        logical_path: &str,
    ) -> Result<(), ReplicaMaterializationError> {
        self.open_or_create_windows_directory(logical_path)
            .map(|_| ())
            .map_err(|source| ReplicaMaterializationError::Write {
                path: self.path.join(logical_path),
                source,
            })
    }

    #[cfg(not(any(unix, windows)))]
    pub(crate) fn create_directory(
        &self,
        logical_path: &str,
    ) -> Result<(), ReplicaMaterializationError> {
        let path = self.path.join(logical_path);
        fs::create_dir_all(&path)
            .map_err(|source| ReplicaMaterializationError::Write { path, source })
    }

    #[cfg(unix)]
    pub(crate) fn write_file<R: Read + ?Sized>(
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

    #[cfg(windows)]
    pub(crate) fn write_file<R: Read + ?Sized>(
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
            .map(|parent| self.open_or_create_windows_directory(parent))
            .transpose()
            .map_err(|source| ReplicaMaterializationError::Write {
                path: target.clone(),
                source,
            })?;
        let directory = parent.as_ref().unwrap_or(&self.root);
        let mut file = directory.create_file(OsStr::new(name)).map_err(|source| {
            ReplicaMaterializationError::Write {
                path: target.clone(),
                source,
            }
        })?;
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
            .and_then(|()| set_windows_file_read_only(&file))
            .and_then(|()| file.sync_all())
            .map_err(|source| ReplicaMaterializationError::Write {
                path: target,
                source,
            })
    }

    #[cfg(not(any(unix, windows)))]
    pub(crate) fn write_file<R: Read + ?Sized>(
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
    pub(crate) fn publish(
        &mut self,
        destination: &Path,
        expected_staging: WorkspaceGenerationIdentity,
    ) -> Result<(), ReplicaMaterializationError> {
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
        if !stat_matches_workspace_identity(&root_identity, expected_staging) {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "workspace staging root identity changed before publication",
            )));
        }
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

        preflight_new_destination_spelling(&self.parent, destination_name).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                ReplicaMaterializationError::DestinationExists(destination.to_path_buf())
            } else {
                ReplicaMaterializationError::Publish(error)
            }
        })?;
        let immediate_parent = verify_anchored_parent(&self.parent, &parent_path)
            .map_err(ReplicaMaterializationError::Publish)?;
        let immediate_root = rustix::fs::fstat(&self.root)
            .map_err(|error| ReplicaMaterializationError::Publish(error.into()))?;
        if !same_file_identity(&parent_identity, &immediate_parent)
            || !stat_matches_workspace_identity(&immediate_root, expected_staging)
            || !named_entry_matches(&self.parent, &self.name, &immediate_root).unwrap_or(false)
        {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "workspace staging root identity changed immediately before publication",
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
        rustix::fs::fsync(&self.root)
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

    #[cfg(windows)]
    pub(crate) fn publish(
        &mut self,
        destination: &Path,
        expected_staging: WorkspaceGenerationIdentity,
    ) -> Result<(), ReplicaMaterializationError> {
        let parent_path = self
            .path
            .parent()
            .ok_or(ReplicaMaterializationError::InvalidDestination)?;
        if destination.parent() != Some(parent_path) {
            return Err(ReplicaMaterializationError::InvalidDestination);
        }
        let destination_name = destination
            .file_name()
            .ok_or(ReplicaMaterializationError::InvalidDestination)?;
        let named_parent = WindowsDirectory::open_absolute(parent_path)
            .map_err(ReplicaMaterializationError::Publish)?;
        if named_parent
            .identity()
            .map_err(ReplicaMaterializationError::Publish)?
            != self
                .parent
                .identity()
                .map_err(ReplicaMaterializationError::Publish)?
            || self
                .root
                .identity()
                .map_err(ReplicaMaterializationError::Publish)?
                != expected_staging
        {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "workspace staging or parent identity changed before publication",
            )));
        }
        match self.parent.open_directory(destination_name) {
            Ok(_) => {
                return Err(ReplicaMaterializationError::DestinationExists(
                    destination.to_path_buf(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(ReplicaMaterializationError::Publish(error)),
        }
        self.root
            .rename_no_replace(&self.parent, destination_name)
            .map_err(ReplicaMaterializationError::Publish)?;
        let published = self
            .parent
            .open_directory(destination_name)
            .map_err(ReplicaMaterializationError::Publish)?;
        if published
            .identity()
            .map_err(ReplicaMaterializationError::Publish)?
            != expected_staging
        {
            return Err(ReplicaMaterializationError::Publish(io::Error::other(
                "workspace root identity changed during publication",
            )));
        }
        self.root
            .set_read_only()
            .and_then(|()| self.root.sync())
            .and_then(|()| self.parent.sync())
            .map_err(ReplicaMaterializationError::Publish)?;
        self.path = destination.to_path_buf();
        self.name = destination_name.to_os_string();
        self.published = true;
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    pub(crate) fn publish(
        &mut self,
        destination: &Path,
        _expected_staging: WorkspaceGenerationIdentity,
    ) -> Result<(), ReplicaMaterializationError> {
        fs::rename(&self.path, destination).map_err(ReplicaMaterializationError::Publish)?;
        set_directory_read_only(destination).map_err(ReplicaMaterializationError::Publish)?;
        self.published = true;
        Ok(())
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if !self.published {
            #[cfg(unix)]
            {
                remove_open_staging_directory(&self.parent, &self.root, &self.name);
            }
            #[cfg(windows)]
            {
                if let (Ok(expected), Ok(named)) =
                    (self.root.identity(), self.parent.open_directory(&self.name))
                    && named.identity().ok() == Some(expected)
                {
                    let _ = self.root.remove_contents(&self.path);
                    if self
                        .parent
                        .open_directory(&self.name)
                        .and_then(|directory| directory.identity())
                        .ok()
                        == Some(expected)
                    {
                        let _ = self.root.mark_delete();
                    }
                }
            }
            #[cfg(not(any(unix, windows)))]
            {
                make_tree_removable(&self.path);
                let _ = fs::remove_dir_all(&self.path);
            }
        }
    }
}

#[cfg(all(test, target_vendor = "apple"))]
mod apple_tests {
    use super::*;

    #[test]
    fn macos_mount_detection_rejects_the_filesystem_root() {
        assert!(workspace_root_is_mount_point(Path::new("/")).expect("inspect root mount"));
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
