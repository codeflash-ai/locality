//! Generation-2 workspace tar validation and planner adapter.
//!
//! The archive is never authority for workspace placement. Scope ordinals in
//! PAX records are resolved through the authenticated session layout, and the
//! complete stream is accepted only after the public pure planner validates it
//! against the sealed offer and terminal control member.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::io::{self, Read};

use locality_core::portable::LogicalPath;
use locality_protocol::workspace_api_v2::{WorkspaceExportOfferV2, WorkspaceProfileSessionV2};
use locality_protocol::workspace_export_v2::{
    WorkspaceArchiveEntryKindV2, WorkspaceArchiveMemberV2, WorkspaceAuthorizedExportEntryV2,
    WorkspaceExportTerminalControlV2, WorkspaceMaterializationPlanV2,
    WorkspaceMaterializationPlanWithInventoryV2, WorkspaceNamespacedInventoryV2,
};
use locality_protocol::{
    DeliveredBodyDigestV2, ExportV2FilePaxMetadata, MAX_EXPORT_TERMINAL_CONTROL_BYTES,
    MAX_EXPORT_V2_FILE_PAX_BYTES, PAX_WINNING_SCOPE_ORDINAL, RESERVED_EXPORT_METADATA_PATH,
};
use sha2::{Digest, Sha256};

const TAR_BLOCK_BYTES: usize = 512;
const READ_ONLY_FILE_MODE: u32 = 0o444;
const READ_ONLY_DIRECTORY_MODE: u32 = 0o555;
const MAX_TAR_EXTENSION_CHAIN: usize = 4;
const MAX_TAR_EXTENSION_BYTES: u64 = (MAX_EXPORT_V2_FILE_PAX_BYTES as u64) * 2;
const MAX_TAR_EXTENSION_CHAIN_BYTES: u64 = MAX_TAR_EXTENSION_BYTES * 2;

struct BoundedTarReader<'a, R> {
    inner: &'a mut R,
    header: [u8; TAR_BLOCK_BYTES],
    header_len: usize,
    header_offset: usize,
    body_remaining: u64,
    extension_count: usize,
    extension_bytes: u64,
}

impl<'a, R> BoundedTarReader<'a, R> {
    fn new(inner: &'a mut R) -> Self {
        Self {
            inner,
            header: [0; TAR_BLOCK_BYTES],
            header_len: 0,
            header_offset: TAR_BLOCK_BYTES,
            body_remaining: 0,
            extension_count: 0,
            extension_bytes: 0,
        }
    }

    fn validate_header(&mut self) -> io::Result<()> {
        if self.header.iter().all(|byte| *byte == 0) {
            if self.extension_count != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "PAX extension is not followed by an archive member",
                ));
            }
            self.body_remaining = 0;
            self.extension_count = 0;
            self.extension_bytes = 0;
            return Ok(());
        }
        let size = parse_raw_tar_size(&self.header[124..136])?;
        self.body_remaining = size
            .checked_add((TAR_BLOCK_BYTES - 1) as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "tar size overflow"))?
            / TAR_BLOCK_BYTES as u64
            * TAR_BLOCK_BYTES as u64;
        match self.header[156] {
            b'x' => {
                if size > MAX_TAR_EXTENSION_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "PAX extension is {size} bytes, exceeding {MAX_TAR_EXTENSION_BYTES}"
                        ),
                    ));
                }
                self.extension_count = self.extension_count.saturating_add(1);
                self.extension_bytes = self.extension_bytes.saturating_add(size);
                if self.extension_count > MAX_TAR_EXTENSION_CHAIN
                    || self.extension_bytes > MAX_TAR_EXTENSION_CHAIN_BYTES
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "PAX extension chain exceeds its metadata bound",
                    ));
                }
            }
            b'g' => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "global PAX extensions are forbidden",
                ));
            }
            b'L' | b'K' => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "GNU long-name and long-link extensions are forbidden",
                ));
            }
            _ => {
                self.extension_count = 0;
                self.extension_bytes = 0;
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for BoundedTarReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.header_offset < TAR_BLOCK_BYTES {
            let available = TAR_BLOCK_BYTES - self.header_offset;
            let copied = available.min(output.len());
            output[..copied].copy_from_slice(
                &self.header[self.header_offset..self.header_offset.saturating_add(copied)],
            );
            self.header_offset += copied;
            return Ok(copied);
        }
        if self.body_remaining > 0 {
            let allowed = usize::try_from(self.body_remaining)
                .unwrap_or(usize::MAX)
                .min(output.len());
            let read = self.inner.read(&mut output[..allowed])?;
            self.body_remaining = self.body_remaining.saturating_sub(read as u64);
            return Ok(read);
        }

        self.header_len = 0;
        while self.header_len < TAR_BLOCK_BYTES {
            let read = self.inner.read(&mut self.header[self.header_len..])?;
            if read == 0 {
                if self.header_len == 0 {
                    return Ok(0);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated tar header block",
                ));
            }
            self.header_len += read;
        }
        self.validate_header()?;
        self.header_offset = 0;
        self.read(output)
    }
}

fn parse_raw_tar_size(field: &[u8]) -> io::Result<u64> {
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "base-256 tar sizes are forbidden",
        ));
    }
    let start = field
        .iter()
        .position(|byte| *byte != b' ')
        .unwrap_or(field.len());
    let end = field
        .iter()
        .rposition(|byte| !matches!(*byte, 0 | b' '))
        .map_or(start, |index| index + 1);
    let digits = &field[start..end];
    if !digits.iter().all(|byte| matches!(*byte, b'0'..=b'7')) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid tar size",
        ));
    }
    digits.iter().try_fold(0_u64, |value, byte| {
        value
            .checked_mul(8)
            .and_then(|value| value.checked_add(u64::from(*byte - b'0')))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "tar size overflow"))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkspaceArchiveLimits {
    pub max_entries: u64,
    pub max_file_bytes: u64,
    pub max_content_bytes: u64,
}

impl Default for WorkspaceArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_file_bytes: 256 * 1024 * 1024,
            max_content_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

/// A staging-only archive destination. Implementations must not make writes
/// visible at the final workspace root from either callback.
pub trait WorkspaceArchiveSink {
    fn create_directory(&mut self, member_path: &str) -> io::Result<()>;

    fn write_file(
        &mut self,
        member_path: &str,
        body: &mut dyn Read,
        expected_size: u64,
    ) -> io::Result<()>;
}

/// Validated result of consuming one complete decoded generation-2 tar stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedWorkspaceArchive {
    pub plan: WorkspaceMaterializationPlanV2,
    pub terminal_control: WorkspaceExportTerminalControlV2,
    pub archive_entries: u64,
    pub files: u64,
    pub directories: u64,
    pub content_bytes: u64,
}

/// Opt-in generation-2 validation result that carries the exact canonical
/// inventory built while producing the materialization plan.
///
/// [`ValidatedWorkspaceArchive`] remains unchanged for source compatibility.
/// New callers that need baseline authority should use
/// [`validate_workspace_tar_with_inventory_v2`] and retain this result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedWorkspaceArchiveWithInventoryV2 {
    validated: ValidatedWorkspaceArchive,
    inventory: WorkspaceNamespacedInventoryV2,
}

impl ValidatedWorkspaceArchiveWithInventoryV2 {
    pub fn validated(&self) -> &ValidatedWorkspaceArchive {
        &self.validated
    }

    pub fn inventory(&self) -> &WorkspaceNamespacedInventoryV2 {
        &self.inventory
    }

    pub fn into_parts(self) -> (ValidatedWorkspaceArchive, WorkspaceNamespacedInventoryV2) {
        (self.validated, self.inventory)
    }
}

#[derive(Debug)]
pub enum WorkspaceArchiveError {
    MalformedTar(String),
    MissingTarEndMarker,
    TrailingTarData,
    EntryLimit { limit: u64 },
    FileLimit { path: String, size: u64, limit: u64 },
    ContentLimit { size: u64, limit: u64 },
    InvalidMember(String),
    InvalidPax(String),
    InvalidControl(String),
    ContentDigestMismatch { path: String },
    DeliveredBodyDigestMismatch,
    Planner(String),
    Sink { path: String, source: io::Error },
}

impl Display for WorkspaceArchiveError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedTar(message) => write!(formatter, "invalid workspace tar: {message}"),
            Self::MissingTarEndMarker => {
                formatter.write_str("invalid workspace tar: missing two-block end marker")
            }
            Self::TrailingTarData => {
                formatter.write_str("invalid workspace tar: trailing data after end marker")
            }
            Self::EntryLimit { limit } => {
                write!(formatter, "workspace entry limit exceeded: {limit}")
            }
            Self::FileLimit { path, size, limit } => write!(
                formatter,
                "workspace file `{path}` is {size} bytes, exceeding limit {limit}"
            ),
            Self::ContentLimit { size, limit } => write!(
                formatter,
                "workspace content is {size} bytes, exceeding limit {limit}"
            ),
            Self::InvalidMember(message) => {
                write!(formatter, "invalid workspace member: {message}")
            }
            Self::InvalidPax(message) => {
                write!(formatter, "invalid workspace PAX metadata: {message}")
            }
            Self::InvalidControl(message) => {
                write!(formatter, "invalid workspace control: {message}")
            }
            Self::ContentDigestMismatch { path } => {
                write!(
                    formatter,
                    "workspace file `{path}` does not match its content digest"
                )
            }
            Self::DeliveredBodyDigestMismatch => {
                formatter.write_str("workspace delivered-body digest does not match the receipt")
            }
            Self::Planner(message) => {
                write!(formatter, "workspace planner rejected archive: {message}")
            }
            Self::Sink { path, source } => {
                write!(
                    formatter,
                    "failed to stage workspace path `{path}`: {source}"
                )
            }
        }
    }
}

impl std::error::Error for WorkspaceArchiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sink { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Consume, stage, and validate one decoded standard tar stream.
///
/// Calls into `sink` occur before the final control member is available, so a
/// sink is required to remain private and rollback-capable until this function
/// returns `Ok`. The returned planner output is the only accepted publication
/// plan.
pub fn validate_workspace_tar<R: Read, S: WorkspaceArchiveSink>(
    reader: &mut R,
    sink: &mut S,
    limits: WorkspaceArchiveLimits,
    session: &WorkspaceProfileSessionV2,
    offer: &WorkspaceExportOfferV2,
) -> Result<ValidatedWorkspaceArchive, WorkspaceArchiveError> {
    validate_workspace_tar_with_inventory_v2(reader, sink, limits, session, offer)
        .map(|result| result.validated)
}

/// Consume, stage, and validate one decoded standard tar stream while
/// retaining the exact canonical generation-2 inventory.
///
/// This opt-in API returns the inventory produced by the same planner call as
/// the materialization plan. It does not reconstruct inventory authority from
/// the validated plan or clone archive authorized entries into a second list.
pub fn validate_workspace_tar_with_inventory_v2<R: Read, S: WorkspaceArchiveSink>(
    reader: &mut R,
    sink: &mut S,
    limits: WorkspaceArchiveLimits,
    session: &WorkspaceProfileSessionV2,
    offer: &WorkspaceExportOfferV2,
) -> Result<ValidatedWorkspaceArchiveWithInventoryV2, WorkspaceArchiveError> {
    session
        .validate()
        .map_err(|error| WorkspaceArchiveError::Planner(error.to_string()))?;
    offer
        .validate()
        .map_err(|error| WorkspaceArchiveError::Planner(error.to_string()))?;

    let layout_by_scope = session
        .session_layout()
        .entries()
        .iter()
        .map(|entry| (entry.scope_ordinal(), (entry.mount_id(), entry.target())))
        .collect::<BTreeMap<_, _>>();
    let targets = session
        .session_layout()
        .entries()
        .iter()
        .map(|entry| (entry.target().as_str(), entry.mount_id()))
        .collect::<BTreeMap<_, _>>();
    let mut members = Vec::new();
    let mut raw_control = None;
    let mut delivered = DeliveredBodyDigestV2::new(offer.offer().file_count);
    let mut files = 0_u64;
    let mut directories = 0_u64;
    let mut content_bytes = 0_u64;
    let mut bounded = BoundedTarReader::new(reader);

    {
        let mut archive = tar::Archive::new(&mut bounded);
        let entries = archive
            .entries()
            .map_err(|error| WorkspaceArchiveError::MalformedTar(error.to_string()))?;
        for entry in entries {
            let mut entry =
                entry.map_err(|error| WorkspaceArchiveError::MalformedTar(error.to_string()))?;
            let next_count = u64::try_from(members.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1);
            if next_count > limits.max_entries {
                return Err(WorkspaceArchiveError::EntryLimit {
                    limit: limits.max_entries,
                });
            }
            if raw_control.is_some() {
                return Err(WorkspaceArchiveError::InvalidControl(
                    "the control member is not final".to_string(),
                ));
            }

            let entry_type = entry.header().entry_type();
            let is_directory = entry_type.is_dir();
            if !entry_type.is_file() && !is_directory {
                return Err(WorkspaceArchiveError::InvalidMember(
                    "links, devices, fifos, and special entries are forbidden".to_string(),
                ));
            }
            if entry.header().link_name_bytes().is_some() {
                return Err(WorkspaceArchiveError::InvalidMember(
                    "link metadata is forbidden".to_string(),
                ));
            }
            let path = archive_member_path(entry.path_bytes().as_ref(), is_directory)?;
            let pax = locality_pax_fields(&mut entry)?;
            let mode = entry
                .header()
                .mode()
                .map_err(|error| WorkspaceArchiveError::MalformedTar(error.to_string()))?;

            if path == RESERVED_EXPORT_METADATA_PATH {
                if is_directory || mode != READ_ONLY_FILE_MODE || !pax.is_empty() {
                    return Err(WorkspaceArchiveError::InvalidControl(
                        "the control member must be a metadata-free 0444 regular file".to_string(),
                    ));
                }
                if entry.size() > MAX_EXPORT_TERMINAL_CONTROL_BYTES as u64 {
                    return Err(WorkspaceArchiveError::InvalidControl(
                        "the control member exceeds its byte limit".to_string(),
                    ));
                }
                let mut bytes = Vec::with_capacity(entry.size() as usize);
                entry
                    .read_to_end(&mut bytes)
                    .map_err(|error| WorkspaceArchiveError::MalformedTar(error.to_string()))?;
                let parsed: WorkspaceExportTerminalControlV2 = serde_json::from_slice(&bytes)
                    .map_err(|error| WorkspaceArchiveError::InvalidControl(error.to_string()))?;
                let canonical = serde_json::to_vec(&parsed)
                    .map_err(|error| WorkspaceArchiveError::InvalidControl(error.to_string()))?;
                if canonical != bytes {
                    return Err(WorkspaceArchiveError::InvalidControl(
                        "the control member is not canonical compact JSON".to_string(),
                    ));
                }
                members.push(WorkspaceArchiveMemberV2 {
                    kind: WorkspaceArchiveEntryKindV2::Control,
                    member_path: path,
                    authorized_entry: None,
                });
                raw_control = Some(parsed);
                continue;
            }

            let (target, logical_path) = split_target_path(&path, &targets)?;
            if is_directory {
                if mode != READ_ONLY_DIRECTORY_MODE || entry.size() != 0 {
                    return Err(WorkspaceArchiveError::InvalidMember(format!(
                        "directory `{path}` must be empty with mode 0555"
                    )));
                }
                let authorized_entry = if let Some(logical_path) = logical_path {
                    let ordinal = directory_scope_ordinal(&pax)?;
                    let (mount_id, expected_target) =
                        layout_by_scope.get(&ordinal).ok_or_else(|| {
                            WorkspaceArchiveError::InvalidPax(format!(
                                "unknown scope ordinal {ordinal}"
                            ))
                        })?;
                    if expected_target.as_str() != target {
                        return Err(WorkspaceArchiveError::InvalidPax(format!(
                            "scope ordinal {ordinal} does not map to target `{target}`"
                        )));
                    }
                    Some(WorkspaceAuthorizedExportEntryV2::Directory {
                        winning_scope_ordinal: ordinal,
                        mount_id: (*mount_id).clone(),
                        logical_path: logical_path.to_string(),
                    })
                } else {
                    if !pax.is_empty() {
                        return Err(WorkspaceArchiveError::InvalidPax(
                            "target root directory has locality metadata".to_string(),
                        ));
                    }
                    None
                };
                sink.create_directory(&path)
                    .map_err(|source| WorkspaceArchiveError::Sink {
                        path: path.clone(),
                        source,
                    })?;
                members.push(WorkspaceArchiveMemberV2 {
                    kind: WorkspaceArchiveEntryKindV2::Directory,
                    member_path: path,
                    authorized_entry,
                });
                directories = directories.saturating_add(1);
                continue;
            }

            let logical_path = logical_path.ok_or_else(|| {
                WorkspaceArchiveError::InvalidMember("target root must be a directory".to_string())
            })?;
            let logical_path = logical_path.to_string();
            if mode != READ_ONLY_FILE_MODE {
                return Err(WorkspaceArchiveError::InvalidMember(format!(
                    "file `{path}` must have mode 0444"
                )));
            }
            let size = entry.size();
            if size > limits.max_file_bytes {
                return Err(WorkspaceArchiveError::FileLimit {
                    path,
                    size,
                    limit: limits.max_file_bytes,
                });
            }
            content_bytes = content_bytes.saturating_add(size);
            if content_bytes > limits.max_content_bytes {
                return Err(WorkspaceArchiveError::ContentLimit {
                    size: content_bytes,
                    limit: limits.max_content_bytes,
                });
            }
            let metadata = ExportV2FilePaxMetadata::from_records(&pax).ok_or_else(|| {
                WorkspaceArchiveError::InvalidPax(format!("file `{path}` has invalid metadata"))
            })?;
            let (mount_id, expected_target) = layout_by_scope
                .get(&metadata.winning_scope_ordinal)
                .ok_or_else(|| {
                    WorkspaceArchiveError::InvalidPax(format!(
                        "unknown scope ordinal {}",
                        metadata.winning_scope_ordinal
                    ))
                })?;
            if expected_target.as_str() != target {
                return Err(WorkspaceArchiveError::InvalidPax(format!(
                    "scope ordinal {} does not map to target `{target}`",
                    metadata.winning_scope_ordinal
                )));
            }
            delivered
                .begin_file(&metadata.projection_id, size)
                .map_err(|error| WorkspaceArchiveError::InvalidPax(error.to_string()))?;
            let mut hashing = WorkspaceBodyReader::new(&mut entry, &mut delivered);
            sink.write_file(&path, &mut hashing, size)
                .map_err(|source| WorkspaceArchiveError::Sink {
                    path: path.clone(),
                    source,
                })?;
            let actual_sha256 = hashing.finish();
            delivered
                .end_file()
                .map_err(|error| WorkspaceArchiveError::InvalidPax(error.to_string()))?;
            if actual_sha256 != metadata.content_sha256 {
                return Err(WorkspaceArchiveError::ContentDigestMismatch { path });
            }
            members.push(WorkspaceArchiveMemberV2 {
                kind: WorkspaceArchiveEntryKindV2::File,
                member_path: path,
                authorized_entry: Some(WorkspaceAuthorizedExportEntryV2::File {
                    winning_scope_ordinal: metadata.winning_scope_ordinal,
                    mount_id: (*mount_id).clone(),
                    logical_path,
                    projection_id: metadata.projection_id,
                    source_connection_id: metadata.source_connection_id,
                    file_kind: metadata.file_kind,
                    effective_actions: metadata.effective_actions,
                    content_sha256: metadata.content_sha256,
                    byte_length: size,
                }),
            });
            files = files.saturating_add(1);
        }
    }

    let mut end_block = [0_u8; TAR_BLOCK_BYTES];
    if bounded.read_exact(&mut end_block).is_err() || end_block.iter().any(|byte| *byte != 0) {
        return Err(WorkspaceArchiveError::MissingTarEndMarker);
    }
    let mut trailing = [0_u8; 1];
    if bounded
        .inner
        .read(&mut trailing)
        .map_err(|error| WorkspaceArchiveError::MalformedTar(error.to_string()))?
        != 0
    {
        return Err(WorkspaceArchiveError::TrailingTarData);
    }

    let terminal_control = raw_control.ok_or_else(|| {
        WorkspaceArchiveError::InvalidControl("the control member is missing".to_string())
    })?;
    let planned = WorkspaceMaterializationPlanWithInventoryV2::plan(
        session,
        offer,
        &terminal_control,
        &members,
    )
    .map_err(|error| WorkspaceArchiveError::Planner(error.to_string()))?;
    let (plan, inventory) = planned.into_parts();
    let delivered_body_sha256 = delivered
        .finish()
        .map_err(|error| WorkspaceArchiveError::Planner(error.to_string()))?;
    if delivered_body_sha256
        != terminal_control
            .completion_receipt
            .receipt
            .delivered_body_sha256
    {
        return Err(WorkspaceArchiveError::DeliveredBodyDigestMismatch);
    }

    Ok(ValidatedWorkspaceArchiveWithInventoryV2 {
        validated: ValidatedWorkspaceArchive {
            plan,
            terminal_control,
            archive_entries: u64::try_from(members.len()).unwrap_or(u64::MAX),
            files,
            directories,
            content_bytes,
        },
        inventory,
    })
}

fn archive_member_path(raw: &[u8], is_directory: bool) -> Result<String, WorkspaceArchiveError> {
    let raw = std::str::from_utf8(raw)
        .map_err(|_| WorkspaceArchiveError::InvalidMember("path is not UTF-8".to_string()))?;
    let path = if is_directory {
        raw.strip_suffix('/').unwrap_or(raw)
    } else {
        raw
    };
    if path == RESERVED_EXPORT_METADATA_PATH {
        return Ok(path.to_string());
    }
    LogicalPath::new(path)
        .map(LogicalPath::into_string)
        .map_err(|error| WorkspaceArchiveError::InvalidMember(error.to_string()))
}

fn split_target_path<'a>(
    path: &'a str,
    targets: &BTreeMap<&str, &locality_core::workspace_layout::PortableMountId>,
) -> Result<(&'a str, Option<&'a str>), WorkspaceArchiveError> {
    let (target, logical_path) = path
        .split_once('/')
        .map_or((path, None), |(target, logical_path)| {
            (target, Some(logical_path))
        });
    if !targets.contains_key(target) {
        return Err(WorkspaceArchiveError::InvalidMember(format!(
            "path `{path}` has unknown target `{target}`"
        )));
    }
    Ok((target, logical_path))
}

fn directory_scope_ordinal(fields: &[(String, String)]) -> Result<u32, WorkspaceArchiveError> {
    if fields.len() != 1 || fields[0].0 != PAX_WINNING_SCOPE_ORDINAL {
        return Err(WorkspaceArchiveError::InvalidPax(
            "directory must carry only locality.winning_scope_ordinal".to_string(),
        ));
    }
    let ordinal = fields[0]
        .1
        .parse::<u32>()
        .map_err(|_| WorkspaceArchiveError::InvalidPax("invalid scope ordinal".to_string()))?;
    if ordinal.to_string() != fields[0].1 {
        return Err(WorkspaceArchiveError::InvalidPax(
            "scope ordinal is not canonical decimal".to_string(),
        ));
    }
    Ok(ordinal)
}

fn locality_pax_fields<R: Read>(
    entry: &mut tar::Entry<'_, R>,
) -> Result<Vec<(String, String)>, WorkspaceArchiveError> {
    let mut fields = Vec::new();
    let Some(extensions) = entry
        .pax_extensions()
        .map_err(|_| WorkspaceArchiveError::InvalidPax("malformed record".to_string()))?
    else {
        return Ok(fields);
    };
    let mut seen = BTreeSet::new();
    for extension in extensions {
        let extension = extension
            .map_err(|_| WorkspaceArchiveError::InvalidPax("malformed record".to_string()))?;
        let key_bytes = extension.key_bytes();
        if !key_bytes.starts_with(b"locality.") {
            continue;
        }
        let key = std::str::from_utf8(key_bytes)
            .map_err(|_| WorkspaceArchiveError::InvalidPax("key is not UTF-8".to_string()))?;
        let value = extension
            .value()
            .map_err(|_| WorkspaceArchiveError::InvalidPax("value is not UTF-8".to_string()))?;
        if !seen.insert(key.to_string()) {
            return Err(WorkspaceArchiveError::InvalidPax(format!(
                "duplicate key `{key}`"
            )));
        }
        fields.push((key.to_string(), value.to_string()));
    }
    Ok(fields)
}

struct WorkspaceBodyReader<'a, R> {
    inner: &'a mut R,
    content: Sha256,
    delivered: &'a mut DeliveredBodyDigestV2,
}

impl<'a, R> WorkspaceBodyReader<'a, R> {
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

impl<R: Read> Read for WorkspaceBodyReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(output)?;
        self.content.update(&output[..read]);
        self.delivered
            .update_file_chunk(&output[..read])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid delivered body"))?;
        Ok(read)
    }
}
