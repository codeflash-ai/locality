//! Staged, durable publication of validated generation-2 workspace archives.

use std::fmt::{Display, Formatter};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use locality_protocol::workspace_api_v2::{WorkspaceExportOfferV2, WorkspaceProfileSessionV2};

use crate::remote_truth::{ReplicaArchive, ReplicaArchiveEncoding};
use crate::replica_materializer::{ReplicaMaterializationError, StagingDirectory};
use crate::workspace_archive::{
    ValidatedWorkspaceArchive, WorkspaceArchiveError, WorkspaceArchiveLimits, WorkspaceArchiveSink,
    validate_workspace_tar,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceMaterializationLimits {
    pub archive: WorkspaceArchiveLimits,
    pub max_decoded_bytes: u64,
    pub max_zstd_window_log: u32,
}

impl Default for WorkspaceMaterializationLimits {
    fn default() -> Self {
        Self {
            archive: WorkspaceArchiveLimits::default(),
            max_decoded_bytes: 4 * 1024 * 1024 * 1024,
            max_zstd_window_log: 23,
        }
    }
}

#[derive(Debug)]
pub enum WorkspaceMaterializationError {
    InvalidDestination,
    DestinationParentMissing(PathBuf),
    DestinationExists(PathBuf),
    Decode(String),
    DecodedLimit { limit: u64 },
    TrailingZstdData,
    Archive(WorkspaceArchiveError),
    Filesystem(ReplicaMaterializationError),
}

impl Display for WorkspaceMaterializationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDestination => {
                formatter.write_str("workspace destination must have a parent and file name")
            }
            Self::DestinationParentMissing(path) => write!(
                formatter,
                "workspace destination parent does not exist: {}",
                path.display()
            ),
            Self::DestinationExists(path) => {
                write!(
                    formatter,
                    "workspace destination already exists: {}",
                    path.display()
                )
            }
            Self::Decode(message) => write!(formatter, "invalid Zstd workspace stream: {message}"),
            Self::DecodedLimit { limit } => {
                write!(formatter, "workspace decoded-byte limit exceeded: {limit}")
            }
            Self::TrailingZstdData => formatter
                .write_str("invalid Zstd workspace stream: multiple frames or trailing data"),
            Self::Archive(error) => Display::fmt(error, formatter),
            Self::Filesystem(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for WorkspaceMaterializationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Archive(error) => Some(error),
            Self::Filesystem(error) => Some(error),
            _ => None,
        }
    }
}

impl From<WorkspaceArchiveError> for WorkspaceMaterializationError {
    fn from(error: WorkspaceArchiveError) -> Self {
        Self::Archive(error)
    }
}

impl From<ReplicaMaterializationError> for WorkspaceMaterializationError {
    fn from(error: ReplicaMaterializationError) -> Self {
        Self::Filesystem(error)
    }
}

pub struct StagedWorkspaceMaterialization {
    staging: StagingDirectory,
    validated: ValidatedWorkspaceArchive,
    decoded_bytes: u64,
}

impl StagedWorkspaceMaterialization {
    pub fn validated(&self) -> &ValidatedWorkspaceArchive {
        &self.validated
    }

    pub fn decoded_bytes(&self) -> u64 {
        self.decoded_bytes
    }

    pub fn staging_path(&self) -> &Path {
        self.staging.path()
    }

    /// Atomically publish a workspace that does not already exist.
    pub fn publish_initial(
        mut self,
        destination: &Path,
    ) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
        if fs::symlink_metadata(destination).is_ok() {
            return Err(WorkspaceMaterializationError::DestinationExists(
                destination.to_path_buf(),
            ));
        }
        self.staging.publish(destination)?;
        self.staging.sync_parent()?;
        Ok(PublishedWorkspace {
            validated: self.validated,
            decoded_bytes: self.decoded_bytes,
            old_generation: None,
        })
    }

    /// Atomically exchange a complete staged generation with an existing root.
    ///
    /// Platforms without a trustworthy directory-exchange primitive return an
    /// unsupported filesystem error without changing the existing root.
    pub fn publish_exchange(
        mut self,
        destination: &Path,
    ) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
        let old_generation = self.staging.exchange(destination)?;
        Ok(PublishedWorkspace {
            validated: self.validated,
            decoded_bytes: self.decoded_bytes,
            old_generation: Some(old_generation),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedWorkspace {
    pub validated: ValidatedWorkspaceArchive,
    pub decoded_bytes: u64,
    /// Sibling path containing the old complete generation after exchange.
    /// Recovery-aware callers retain it until the new receipt is durable.
    pub old_generation: Option<PathBuf>,
}

/// Decode and validate an export entirely inside a no-follow sibling staging
/// directory. No final workspace path is modified by this function.
pub fn stage_workspace_archive<Body: Read>(
    archive: ReplicaArchive<Body>,
    destination: &Path,
    limits: WorkspaceMaterializationLimits,
    session: &WorkspaceProfileSessionV2,
    offer: &WorkspaceExportOfferV2,
) -> Result<StagedWorkspaceMaterialization, WorkspaceMaterializationError> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(WorkspaceMaterializationError::InvalidDestination)?;
    if destination.file_name().is_none() {
        return Err(WorkspaceMaterializationError::InvalidDestination);
    }
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) | Err(_) => {
            return Err(WorkspaceMaterializationError::DestinationParentMissing(
                parent.to_path_buf(),
            ));
        }
    }

    let staging = StagingDirectory::create(parent)?;
    let mut sink = StagingSink { staging: &staging };
    let (validated, decoded_bytes) = match archive.encoding {
        ReplicaArchiveEncoding::Identity => {
            let mut decoded = DecodedByteLimitReader::new(archive.body, limits.max_decoded_bytes);
            let result =
                validate_workspace_tar(&mut decoded, &mut sink, limits.archive, session, offer);
            let exceeded = decoded.exceeded;
            let decoded_bytes = decoded.consumed;
            if exceeded {
                return Err(WorkspaceMaterializationError::DecodedLimit {
                    limit: limits.max_decoded_bytes,
                });
            }
            (result?, decoded_bytes)
        }
        ReplicaArchiveEncoding::Zstd => {
            let mut decoder = zstd::stream::read::Decoder::new(archive.body)
                .map_err(|error| WorkspaceMaterializationError::Decode(error.to_string()))?;
            decoder
                .window_log_max(limits.max_zstd_window_log)
                .map_err(|error| WorkspaceMaterializationError::Decode(error.to_string()))?;
            let mut decoder = decoder.single_frame();
            let (result, exceeded, decoded_bytes) = {
                let mut decoded =
                    DecodedByteLimitReader::new(&mut decoder, limits.max_decoded_bytes);
                let result =
                    validate_workspace_tar(&mut decoded, &mut sink, limits.archive, session, offer);
                (result, decoded.exceeded, decoded.consumed)
            };
            if exceeded {
                return Err(WorkspaceMaterializationError::DecodedLimit {
                    limit: limits.max_decoded_bytes,
                });
            }
            let validated = result?;
            let mut compressed = decoder.finish();
            let mut trailing = [0_u8; 1];
            if compressed
                .read(&mut trailing)
                .map_err(|error| WorkspaceMaterializationError::Decode(error.to_string()))?
                != 0
            {
                return Err(WorkspaceMaterializationError::TrailingZstdData);
            }
            (validated, decoded_bytes)
        }
    };
    staging.finalize_durable()?;
    Ok(StagedWorkspaceMaterialization {
        staging,
        validated,
        decoded_bytes,
    })
}

struct StagingSink<'a> {
    staging: &'a StagingDirectory,
}

impl WorkspaceArchiveSink for StagingSink<'_> {
    fn create_directory(&mut self, member_path: &str) -> io::Result<()> {
        self.staging
            .create_directory(member_path)
            .map_err(io::Error::other)
    }

    fn write_file(
        &mut self,
        member_path: &str,
        body: &mut dyn Read,
        expected_size: u64,
    ) -> io::Result<()> {
        self.staging
            .write_file(member_path, body, expected_size)
            .map_err(io::Error::other)
    }
}

struct DecodedByteLimitReader<R> {
    inner: R,
    limit: u64,
    consumed: u64,
    exceeded: bool,
}

impl<R> DecodedByteLimitReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            consumed: 0,
            exceeded: false,
        }
    }
}

impl<R: Read> Read for DecodedByteLimitReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let remaining = self.limit.saturating_sub(self.consumed);
        let allowed = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
            .min(output.len());
        let read = self.inner.read(&mut output[..allowed])?;
        self.consumed = self.consumed.saturating_add(read as u64);
        if self.consumed > self.limit {
            self.exceeded = true;
        }
        Ok(read)
    }
}
