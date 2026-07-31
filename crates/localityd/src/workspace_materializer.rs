//! Staged, durable publication of validated generation-2 workspace archives.

use std::fmt::{Display, Formatter};
use std::fs;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use rustix::fs::{AtFlags, Mode, OFlags};

use hmac::{Hmac, Mac};
use locality_protocol::workspace_api_v2::{WorkspaceExportOfferV2, WorkspaceProfileSessionV2};
use locality_protocol::workspace_export_v2::WorkspaceExportTerminalControlV2;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::remote_truth::{ReplicaArchive, ReplicaArchiveEncoding};
use crate::replica_materializer::{
    ReplicaMaterializationError, StagingDirectory, WorkspaceGenerationFileBinding,
    WorkspaceGenerationIdentity, WorkspacePublicationLock, acquire_workspace_publication_lock,
};
#[cfg(not(unix))]
use crate::replica_materializer::{
    remove_workspace_generation, repair_workspace_generation, workspace_generation_file_binding,
    workspace_generation_identity_if_exists,
};
#[cfg(unix)]
use crate::replica_materializer::{
    remove_workspace_generation_at, repair_workspace_generation_at,
    workspace_generation_file_binding_at, workspace_generation_identity_if_exists_at,
};
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
    Journal { path: PathBuf, source: io::Error },
    RecoveryRequired(PathBuf),
    RecoveryConflict(String),
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
            Self::Journal { path, source } => {
                write!(
                    formatter,
                    "workspace publication state `{}` failed: {source}",
                    path.display()
                )
            }
            Self::RecoveryRequired(path) => write!(
                formatter,
                "workspace publication recovery is required for `{}`",
                path.display()
            ),
            Self::RecoveryConflict(message) => {
                write!(
                    formatter,
                    "workspace publication recovery conflict: {message}"
                )
            }
        }
    }
}

impl std::error::Error for WorkspaceMaterializationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Archive(error) => Some(error),
            Self::Filesystem(error) => Some(error),
            Self::Journal { source, .. } => Some(source),
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
        self,
        destination: &Path,
    ) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
        let paths = PublicationPaths::new(destination)?;
        let lock = paths.acquire_lock()?;
        self.staging.verify_publication_parent(&lock)?;
        let expected_staging = self.staging.identity()?;
        self.publish_initial_expected(destination, expected_staging, &lock, &paths)
    }

    fn publish_initial_expected(
        mut self,
        destination: &Path,
        expected_staging: WorkspaceGenerationIdentity,
        lock: &WorkspacePublicationLock,
        paths: &PublicationPaths,
    ) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
        if generation_identity_if_exists_locked(lock, paths, &paths.destination_name)?.is_some() {
            return Err(WorkspaceMaterializationError::DestinationExists(
                destination.to_path_buf(),
            ));
        }
        self.staging
            .publish_portable_workspace(destination, expected_staging)?;
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
        self,
        destination: &Path,
    ) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
        let paths = PublicationPaths::new(destination)?;
        let lock = paths.acquire_lock()?;
        self.staging.verify_publication_parent(&lock)?;
        let expected_staging = self.staging.identity()?;
        let expected_destination =
            generation_identity_locked(&lock, &paths, &paths.destination_name)?
                .ok_or_else(|| {
                    WorkspaceMaterializationError::DestinationExists(destination.into())
                })?
                .into();
        self.publish_exchange_expected(destination, expected_staging, expected_destination, &lock)
    }

    fn publish_exchange_expected(
        mut self,
        destination: &Path,
        expected_staging: WorkspaceGenerationIdentity,
        expected_destination: WorkspaceGenerationIdentity,
        lock: &WorkspacePublicationLock,
    ) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
        self.staging.verify_publication_parent(lock)?;
        let old_generation =
            self.staging
                .exchange(destination, expected_staging, expected_destination)?;
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

/// Durable local receipt consumed by hosted, Desktop, and `loc` callers. It
/// repeats the complete authenticated terminal control but never stores an
/// absolute host binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePublicationReceipt {
    pub version: u16,
    pub terminal_control: WorkspaceExportTerminalControlV2,
    pub decoded_bytes: u64,
    generation_identity: GenerationIdentity,
    ownership_marker_identity: GenerationIdentity,
    ownership_marker_nonce: String,
    ownership_tag: String,
}

/// Secret capability that authorizes replacement and cleanup of one caller's
/// locally persisted workspace generations. Callers derive it from the
/// reusable Workspace Profile key and never persist it beside the workspace.
#[derive(Clone)]
pub struct WorkspaceOwnershipCapability([u8; 32]);

impl WorkspaceOwnershipCapability {
    pub fn new(secret: [u8; 32]) -> Self {
        Self(secret)
    }
}

impl std::fmt::Debug for WorkspaceOwnershipCapability {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WorkspaceOwnershipCapability(<redacted>)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspacePublicationCheckpoint {
    JournalDurable,
    PublicationComplete,
    ReceiptDurable,
    CleanupComplete,
}

/// Fault-injection and host-observation seam. Production callers use `()`;
/// tests can simulate process loss or storage errors at durable boundaries.
pub trait WorkspacePublicationHooks {
    fn checkpoint(&mut self, checkpoint: WorkspacePublicationCheckpoint) -> io::Result<()>;
}

impl WorkspacePublicationHooks for () {
    fn checkpoint(&mut self, _checkpoint: WorkspacePublicationCheckpoint) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationJournal {
    version: u16,
    destination_name: String,
    staging_name: String,
    new_identity: Option<GenerationIdentity>,
    old_identity: Option<GenerationIdentity>,
    old_receipt: Option<WorkspacePublicationReceipt>,
    new_receipt: WorkspacePublicationReceipt,
    ownership_tag: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationIdentity {
    device: u64,
    inode: u64,
    #[serde(default)]
    inode_high: u64,
}

const PUBLICATION_STATE_VERSION: u16 = 4;
const WORKSPACE_OWNERSHIP_MARKER: &str = ".locality-ownership-v4";
const WORKSPACE_OWNERSHIP_NONCE_BYTES: usize = 32;
const RECEIPT_AUTH_DOMAIN: &str = "locality.workspace-publication-receipt.v4";
const JOURNAL_AUTH_DOMAIN: &str = "locality.workspace-publication-journal.v4";

#[derive(Clone, Debug, PartialEq, Eq)]
struct GenerationMarkerBinding {
    identity: GenerationIdentity,
    nonce: String,
}

impl From<GenerationIdentity> for WorkspaceGenerationIdentity {
    fn from(identity: GenerationIdentity) -> Self {
        Self {
            device: identity.device,
            inode: identity.inode,
            inode_high: identity.inode_high,
        }
    }
}

impl From<WorkspaceGenerationIdentity> for GenerationIdentity {
    fn from(identity: WorkspaceGenerationIdentity) -> Self {
        Self {
            device: identity.device,
            inode: identity.inode,
            inode_high: identity.inode_high,
        }
    }
}

/// Complete generation-2 materialization entry point for Desktop, `loc`, and
/// hosted callers. Existing roots refresh only through atomic exchange; a
/// platform without that primitive returns safely with the old root intact.
pub fn materialize_workspace_archive_durable<Body: Read>(
    archive: ReplicaArchive<Body>,
    destination: &Path,
    limits: WorkspaceMaterializationLimits,
    session: &WorkspaceProfileSessionV2,
    offer: &WorkspaceExportOfferV2,
    ownership: &WorkspaceOwnershipCapability,
) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
    let paths = PublicationPaths::new(destination)?;
    let lock = paths.acquire_lock()?;
    recover_workspace_publication_locked(destination, ownership, &paths, &lock)?;
    let staged =
        stage_workspace_archive_locked(archive, destination, limits, session, offer, Some(&lock))?;
    publish_staged_workspace_locked(staged, destination, ownership, &mut (), &paths, &lock)
}

pub fn publish_staged_workspace(
    staged: StagedWorkspaceMaterialization,
    destination: &Path,
    ownership: &WorkspaceOwnershipCapability,
) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
    publish_staged_workspace_with_hooks(staged, destination, ownership, &mut ())
}

pub fn publish_staged_workspace_with_hooks<H: WorkspacePublicationHooks>(
    staged: StagedWorkspaceMaterialization,
    destination: &Path,
    ownership: &WorkspaceOwnershipCapability,
    hooks: &mut H,
) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
    let paths = PublicationPaths::new(destination)?;
    let lock = paths.acquire_lock()?;
    staged.staging.verify_publication_parent(&lock)?;
    publish_staged_workspace_locked(staged, destination, ownership, hooks, &paths, &lock)
}

fn publish_staged_workspace_locked<H: WorkspacePublicationHooks>(
    staged: StagedWorkspaceMaterialization,
    destination: &Path,
    ownership: &WorkspaceOwnershipCapability,
    hooks: &mut H,
    paths: &PublicationPaths,
    lock: &WorkspacePublicationLock,
) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
    if publication_entry_exists(lock, paths, &paths.journal_name, &paths.journal)? {
        return Err(WorkspaceMaterializationError::RecoveryRequired(
            paths.journal.clone(),
        ));
    }
    let destination_exists =
        generation_identity_if_exists_locked(lock, paths, &paths.destination_name)?.is_some();
    let new_identity: GenerationIdentity = staged.staging.identity()?.into();
    let new_marker = marker_binding_from_file(
        staged
            .staging
            .file_binding(WORKSPACE_OWNERSHIP_MARKER, WORKSPACE_OWNERSHIP_NONCE_BYTES)?,
    )?;
    let old_identity = if destination_exists {
        generation_identity_locked(lock, paths, &paths.destination_name)?
    } else {
        None
    };
    let old_receipt = if let Some(identity) = old_identity {
        let receipt = load_workspace_publication_receipt_locked(paths, lock)?.ok_or_else(|| {
            WorkspaceMaterializationError::RecoveryConflict(
                "an existing workspace root has no durable active receipt".to_string(),
            )
        })?;
        validate_receipt_binding(
            &receipt,
            identity,
            &generation_marker_binding_locked(lock, paths, &paths.destination_name)?,
            &paths.destination_name,
            ownership,
            "active workspace receipt",
        )?;
        Some(receipt)
    } else {
        None
    };
    let staging_name = path_file_name(staged.staging.path())?;
    let mut new_receipt = WorkspacePublicationReceipt {
        version: PUBLICATION_STATE_VERSION,
        terminal_control: staged.validated.terminal_control.clone(),
        decoded_bytes: staged.decoded_bytes,
        generation_identity: new_identity,
        ownership_marker_identity: new_marker.identity,
        ownership_marker_nonce: new_marker.nonce,
        ownership_tag: String::new(),
    };
    new_receipt.ownership_tag =
        receipt_ownership_tag(&new_receipt, &paths.destination_name, ownership)?;
    let mut journal = PublicationJournal {
        version: PUBLICATION_STATE_VERSION,
        destination_name: paths.destination_name.clone(),
        staging_name,
        new_identity: Some(new_identity),
        old_identity,
        old_receipt,
        new_receipt,
        ownership_tag: String::new(),
    };
    journal.ownership_tag = journal_ownership_tag(&journal, ownership)?;
    validate_journal_receipt_bindings(&journal, ownership)?;
    create_durable_journal(paths, lock, &journal)?;
    if let Err(source) = hooks.checkpoint(WorkspacePublicationCheckpoint::JournalDurable) {
        return Err(WorkspaceMaterializationError::Journal {
            path: paths.journal.clone(),
            source,
        });
    }

    validate_receipt_binding(
        &journal.new_receipt,
        required_identity(journal.new_identity, "new journal generation")?,
        &staging_marker_binding(&staged.staging)?,
        &paths.destination_name,
        ownership,
        "staged generation immediately before publication",
    )?;
    if destination_exists {
        validate_receipt_binding(
            journal.old_receipt.as_ref().ok_or_else(|| {
                WorkspaceMaterializationError::RecoveryConflict(
                    "refresh journal has no old-generation receipt".to_string(),
                )
            })?,
            required_identity(journal.old_identity, "old journal generation")?,
            &generation_marker_binding_locked(lock, paths, &paths.destination_name)?,
            &paths.destination_name,
            ownership,
            "active generation immediately before exchange",
        )?;
    }

    let publication = if destination_exists {
        validate_journal_receipt_bindings(&journal, ownership)?;
        staged.publish_exchange_expected(
            destination,
            required_identity(journal.new_identity, "new journal generation")?.into(),
            required_identity(journal.old_identity, "old journal generation")?.into(),
            lock,
        )
    } else {
        staged.publish_initial_expected(
            destination,
            required_identity(journal.new_identity, "new journal generation")?.into(),
            lock,
            paths,
        )
    };
    let published = match publication {
        Ok(published) => published,
        Err(error) => {
            // Publication primitives can fail after an exchange has committed
            // (for example while syncing the parent). Let identity-based
            // recovery distinguish that state from a pre-exchange failure.
            recover_workspace_publication_locked(destination, ownership, paths, lock)?;
            return Err(error);
        }
    };
    if let Err(source) = hooks.checkpoint(WorkspacePublicationCheckpoint::PublicationComplete) {
        return Err(WorkspaceMaterializationError::Journal {
            path: paths.journal.clone(),
            source,
        });
    }
    replace_durable_receipt(paths, lock, &journal.new_receipt, ownership)?;
    if let Err(source) = hooks.checkpoint(WorkspacePublicationCheckpoint::ReceiptDurable) {
        return Err(WorkspaceMaterializationError::Journal {
            path: paths.journal.clone(),
            source,
        });
    }
    if let Some(old_generation) = &published.old_generation {
        validate_receipt_binding(
            journal.old_receipt.as_ref().ok_or_else(|| {
                WorkspaceMaterializationError::RecoveryConflict(
                    "refresh journal has no old-generation receipt".to_string(),
                )
            })?,
            required_identity(journal.old_identity, "old journal generation")?,
            &generation_marker_binding_locked(
                lock,
                paths,
                path_file_name(old_generation)?.as_str(),
            )?,
            &paths.destination_name,
            ownership,
            "old-generation cleanup receipt",
        )?;
        remove_generation_locked(
            lock,
            paths,
            path_file_name(old_generation)?.as_str(),
            required_identity(journal.old_identity, "old journal generation")?,
            journal.old_receipt.as_ref().expect("validated old receipt"),
        )?;
    }
    if let Err(source) = hooks.checkpoint(WorkspacePublicationCheckpoint::CleanupComplete) {
        return Err(WorkspaceMaterializationError::Journal {
            path: paths.journal.clone(),
            source,
        });
    }
    remove_journal_if_present(paths, lock)?;
    Ok(published)
}

/// Recover an interrupted initial publication or refresh using the durable
/// journal plus filesystem identities. It either keeps the old complete root
/// or completes the new receipt; it never merges generations in place.
pub fn recover_workspace_publication(
    destination: &Path,
    ownership: &WorkspaceOwnershipCapability,
) -> Result<(), WorkspaceMaterializationError> {
    let paths = PublicationPaths::new(destination)?;
    let lock = paths.acquire_lock()?;
    recover_workspace_publication_locked(destination, ownership, &paths, &lock)
}

fn recover_workspace_publication_locked(
    destination: &Path,
    ownership: &WorkspaceOwnershipCapability,
    paths: &PublicationPaths,
    lock: &WorkspacePublicationLock,
) -> Result<(), WorkspaceMaterializationError> {
    if !publication_entry_exists(lock, paths, &paths.journal_name, &paths.journal)? {
        return Ok(());
    }
    let journal: PublicationJournal =
        read_json_locked(lock, paths, &paths.journal_name, &paths.journal)?;
    if journal.version != PUBLICATION_STATE_VERSION
        || journal.destination_name != path_file_name(destination)?
    {
        return Err(WorkspaceMaterializationError::RecoveryConflict(
            "journal version or destination binding is invalid".to_string(),
        ));
    }
    validate_journal_receipt_bindings(&journal, ownership)?;
    if !journal.staging_name.starts_with(".locality-stage-")
        || journal.staging_name.contains(['/', '\\'])
    {
        return Err(WorkspaceMaterializationError::RecoveryConflict(
            "journal staging name is invalid".to_string(),
        ));
    }
    let destination_identity =
        generation_identity_if_exists_locked(lock, paths, &paths.destination_name)?;
    let staging_identity =
        generation_identity_if_exists_locked(lock, paths, &journal.staging_name)?;
    let active_receipt = load_workspace_publication_receipt_locked(paths, lock)?;

    if journal.old_identity.is_none() {
        if identity_matches(destination_identity, journal.new_identity) {
            validate_receipt_binding(
                &journal.new_receipt,
                required_identity(journal.new_identity, "new generation")?,
                &generation_marker_binding_locked(lock, paths, &paths.destination_name)?,
                &paths.destination_name,
                ownership,
                "initial publication generation",
            )?;
            if let Some(receipt) = active_receipt.as_ref() {
                validate_active_receipt_matches(
                    receipt,
                    &[(
                        &journal.new_receipt,
                        journal.new_receipt.generation_identity,
                    )],
                    &paths.destination_name,
                    ownership,
                    "initial publication receipt",
                )?;
            }
            repair_generation_locked(
                lock,
                paths,
                &paths.destination_name,
                required_identity(journal.new_identity, "new generation")?,
            )?;
            replace_durable_receipt(paths, lock, &journal.new_receipt, ownership)?;
            if staging_identity.is_some() {
                return Err(WorkspaceMaterializationError::RecoveryConflict(
                    "initial publication left an unexpected staging generation".to_string(),
                ));
            }
        } else if identity_matches(staging_identity, journal.new_identity)
            && destination_identity.is_none()
        {
            if active_receipt.is_some() {
                return Err(WorkspaceMaterializationError::RecoveryConflict(
                    "unpublished initial generation has a stale active receipt".to_string(),
                ));
            }
            validate_receipt_binding(
                &journal.new_receipt,
                required_identity(journal.new_identity, "new generation")?,
                &generation_marker_binding_locked(lock, paths, &journal.staging_name)?,
                &paths.destination_name,
                ownership,
                "unpublished initial generation",
            )?;
            remove_generation_locked(
                lock,
                paths,
                &journal.staging_name,
                required_identity(journal.new_identity, "new generation")?,
                &journal.new_receipt,
            )?;
        } else if destination_identity.is_some()
            || staging_identity.is_some()
            || active_receipt.is_some()
        {
            return Err(WorkspaceMaterializationError::RecoveryConflict(
                "initial publication paths do not match the durable journal".to_string(),
            ));
        }
        remove_journal_if_present(paths, lock)?;
        return Ok(());
    }

    if identity_matches(destination_identity, journal.new_identity)
        && identity_matches(staging_identity, journal.old_identity)
    {
        validate_receipt_binding(
            &journal.new_receipt,
            required_identity(journal.new_identity, "new generation")?,
            &generation_marker_binding_locked(lock, paths, &paths.destination_name)?,
            &paths.destination_name,
            ownership,
            "exchanged new generation",
        )?;
        validate_receipt_binding(
            journal.old_receipt.as_ref().expect("validated old receipt"),
            required_identity(journal.old_identity, "old generation")?,
            &generation_marker_binding_locked(lock, paths, &journal.staging_name)?,
            &paths.destination_name,
            ownership,
            "exchanged old generation",
        )?;
        validate_active_receipt_matches(
            active_receipt.as_ref().ok_or_else(|| {
                WorkspaceMaterializationError::RecoveryConflict(
                    "exchanged workspace has no active receipt".to_string(),
                )
            })?,
            &[
                (
                    journal.old_receipt.as_ref().expect("validated old receipt"),
                    required_identity(journal.old_identity, "old generation")?,
                ),
                (
                    &journal.new_receipt,
                    required_identity(journal.new_identity, "new generation")?,
                ),
            ],
            &paths.destination_name,
            ownership,
            "exchanged workspace receipt",
        )?;
        repair_generation_locked(
            lock,
            paths,
            &paths.destination_name,
            required_identity(journal.new_identity, "new generation")?,
        )?;
        replace_durable_receipt(paths, lock, &journal.new_receipt, ownership)?;
        validate_receipt_binding(
            journal.old_receipt.as_ref().expect("validated old receipt"),
            required_identity(journal.old_identity, "old generation")?,
            &generation_marker_binding_locked(lock, paths, &journal.staging_name)?,
            &paths.destination_name,
            ownership,
            "recovery cleanup receipt",
        )?;
        remove_generation_locked(
            lock,
            paths,
            &journal.staging_name,
            required_identity(journal.old_identity, "old generation")?,
            journal.old_receipt.as_ref().expect("validated old receipt"),
        )?;
        remove_journal_if_present(paths, lock)?;
        return Ok(());
    }
    if identity_matches(destination_identity, journal.old_identity)
        && identity_matches(staging_identity, journal.new_identity)
    {
        validate_receipt_binding(
            journal.old_receipt.as_ref().expect("validated old receipt"),
            required_identity(journal.old_identity, "old generation")?,
            &generation_marker_binding_locked(lock, paths, &paths.destination_name)?,
            &paths.destination_name,
            ownership,
            "pre-exchange old generation",
        )?;
        validate_receipt_binding(
            &journal.new_receipt,
            required_identity(journal.new_identity, "new generation")?,
            &generation_marker_binding_locked(lock, paths, &journal.staging_name)?,
            &paths.destination_name,
            ownership,
            "pre-exchange new generation",
        )?;
        validate_active_receipt_matches(
            active_receipt.as_ref().ok_or_else(|| {
                WorkspaceMaterializationError::RecoveryConflict(
                    "pre-exchange workspace has no active receipt".to_string(),
                )
            })?,
            &[(
                journal.old_receipt.as_ref().expect("validated old receipt"),
                required_identity(journal.old_identity, "old generation")?,
            )],
            &paths.destination_name,
            ownership,
            "pre-exchange workspace receipt",
        )?;
        repair_generation_locked(
            lock,
            paths,
            &paths.destination_name,
            required_identity(journal.old_identity, "old generation")?,
        )?;
        remove_generation_locked(
            lock,
            paths,
            &journal.staging_name,
            required_identity(journal.new_identity, "new generation")?,
            &journal.new_receipt,
        )?;
        remove_journal_if_present(paths, lock)?;
        return Ok(());
    }
    if identity_matches(destination_identity, journal.old_identity) && staging_identity.is_none() {
        validate_receipt_binding(
            journal.old_receipt.as_ref().expect("validated old receipt"),
            required_identity(journal.old_identity, "old generation")?,
            &generation_marker_binding_locked(lock, paths, &paths.destination_name)?,
            &paths.destination_name,
            ownership,
            "rolled-back generation",
        )?;
        validate_active_receipt_matches(
            active_receipt.as_ref().ok_or_else(|| {
                WorkspaceMaterializationError::RecoveryConflict(
                    "rolled-back workspace has no active receipt".to_string(),
                )
            })?,
            &[(
                journal.old_receipt.as_ref().expect("validated old receipt"),
                required_identity(journal.old_identity, "old generation")?,
            )],
            &paths.destination_name,
            ownership,
            "rolled-back workspace receipt",
        )?;
        repair_generation_locked(
            lock,
            paths,
            &paths.destination_name,
            required_identity(journal.old_identity, "old generation")?,
        )?;
        remove_journal_if_present(paths, lock)?;
        return Ok(());
    }
    if identity_matches(destination_identity, journal.new_identity) && staging_identity.is_none() {
        validate_receipt_binding(
            &journal.new_receipt,
            required_identity(journal.new_identity, "new generation")?,
            &generation_marker_binding_locked(lock, paths, &paths.destination_name)?,
            &paths.destination_name,
            ownership,
            "post-cleanup generation",
        )?;
        validate_active_receipt_matches(
            active_receipt.as_ref().ok_or_else(|| {
                WorkspaceMaterializationError::RecoveryConflict(
                    "new generation cleanup completed without its durable receipt".to_string(),
                )
            })?,
            &[(
                &journal.new_receipt,
                required_identity(journal.new_identity, "new generation")?,
            )],
            &paths.destination_name,
            ownership,
            "post-cleanup workspace receipt",
        )?;
        repair_generation_locked(
            lock,
            paths,
            &paths.destination_name,
            required_identity(journal.new_identity, "new generation")?,
        )?;
        remove_journal_if_present(paths, lock)?;
        return Ok(());
    }
    Err(WorkspaceMaterializationError::RecoveryConflict(
        "refresh roots do not match either complete journaled generation".to_string(),
    ))
}

pub fn load_workspace_publication_receipt(
    destination: &Path,
) -> Result<Option<WorkspacePublicationReceipt>, WorkspaceMaterializationError> {
    let paths = PublicationPaths::new(destination)?;
    let lock = paths.acquire_lock()?;
    load_workspace_publication_receipt_locked(&paths, &lock)
}

fn load_workspace_publication_receipt_locked(
    paths: &PublicationPaths,
    lock: &WorkspacePublicationLock,
) -> Result<Option<WorkspacePublicationReceipt>, WorkspaceMaterializationError> {
    if !publication_entry_exists(lock, paths, &paths.receipt_name, &paths.receipt)? {
        return Ok(None);
    }
    read_json_locked(lock, paths, &paths.receipt_name, &paths.receipt).map(Some)
}

/// Recovers any local publication journal and confirms that an existing root
/// is the exact filesystem generation named by its durable generation-2 receipt.
pub fn recover_and_verify_workspace_publication_state(
    destination: &Path,
    ownership: &WorkspaceOwnershipCapability,
) -> Result<bool, WorkspaceMaterializationError> {
    let paths = PublicationPaths::new(destination)?;
    let lock = paths.acquire_lock()?;
    if publication_entry_exists(&lock, &paths, &paths.journal_name, &paths.journal)? {
        recover_workspace_publication_locked(destination, ownership, &paths, &lock)?;
    }
    let Some(identity) =
        generation_identity_if_exists_locked(&lock, &paths, &paths.destination_name)?
    else {
        if publication_entry_exists(&lock, &paths, &paths.receipt_name, &paths.receipt)? {
            return Err(WorkspaceMaterializationError::RecoveryConflict(
                "workspace receipt exists without its published generation".to_string(),
            ));
        }
        return Ok(false);
    };
    let Some(receipt) = load_workspace_publication_receipt_locked(&paths, &lock)? else {
        return Ok(false);
    };
    validate_receipt_binding(
        &receipt,
        identity,
        &generation_marker_binding_locked(&lock, &paths, &paths.destination_name)?,
        &paths.destination_name,
        ownership,
        "active workspace receipt",
    )?;
    Ok(true)
}

struct PublicationPaths {
    parent: PathBuf,
    destination_name: String,
    journal_name: String,
    journal: PathBuf,
    receipt_name: String,
    receipt: PathBuf,
    lock_name: String,
}

impl PublicationPaths {
    fn new(destination: &Path) -> Result<Self, WorkspaceMaterializationError> {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or(WorkspaceMaterializationError::InvalidDestination)?
            .to_path_buf();
        let destination_name = path_file_name(destination)?;
        let journal_name = format!(".locality-{destination_name}.publication.json");
        let receipt_name = format!(".locality-{destination_name}.receipt.json");
        Ok(Self {
            journal: parent.join(&journal_name),
            journal_name,
            receipt: parent.join(&receipt_name),
            receipt_name,
            lock_name: format!(".locality-{destination_name}.publication.lock"),
            destination_name,
            parent,
        })
    }

    fn acquire_lock(&self) -> Result<WorkspacePublicationLock, WorkspaceMaterializationError> {
        acquire_workspace_publication_lock(&self.parent, &self.lock_name).map_err(|source| {
            WorkspaceMaterializationError::Journal {
                path: self.parent.join(&self.lock_name),
                source,
            }
        })
    }
}

fn create_durable_journal(
    paths: &PublicationPaths,
    lock: &WorkspacePublicationLock,
    journal: &PublicationJournal,
) -> Result<(), WorkspaceMaterializationError> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|error| WorkspaceMaterializationError::RecoveryConflict(error.to_string()))?;
    let temporary = write_durable_temporary(paths, lock, "journal", &bytes)?;
    #[cfg(unix)]
    let link_result = rustix::fs::linkat(
        lock.parent_directory(),
        &temporary.name,
        lock.parent_directory(),
        &paths.journal_name,
        AtFlags::empty(),
    )
    .map_err(io::Error::from);
    #[cfg(not(unix))]
    let link_result = fs::hard_link(&temporary.path, &paths.journal);
    remove_temporary(lock, &temporary);
    link_result.map_err(|source| WorkspaceMaterializationError::Journal {
        path: paths.journal.clone(),
        source,
    })?;
    sync_publication_parent(lock, &paths.parent).map_err(|source| {
        WorkspaceMaterializationError::Journal {
            path: paths.parent.clone(),
            source,
        }
    })
}

fn replace_durable_receipt(
    paths: &PublicationPaths,
    lock: &WorkspacePublicationLock,
    receipt: &WorkspacePublicationReceipt,
    ownership: &WorkspaceOwnershipCapability,
) -> Result<(), WorkspaceMaterializationError> {
    let identity = generation_identity_if_exists_locked(lock, paths, &paths.destination_name)?
        .ok_or_else(|| {
            WorkspaceMaterializationError::RecoveryConflict(
                "cannot publish a receipt without its workspace generation".to_string(),
            )
        })?;
    validate_receipt_binding(
        receipt,
        identity,
        &generation_marker_binding_locked(lock, paths, &paths.destination_name)?,
        &paths.destination_name,
        ownership,
        "durable workspace receipt",
    )?;
    let bytes = serde_json::to_vec(receipt)
        .map_err(|error| WorkspaceMaterializationError::RecoveryConflict(error.to_string()))?;
    let temporary = write_durable_temporary(paths, lock, "receipt", &bytes)?;
    #[cfg(unix)]
    let rename_result = rustix::fs::renameat(
        lock.parent_directory(),
        &temporary.name,
        lock.parent_directory(),
        &paths.receipt_name,
    )
    .map_err(io::Error::from);
    #[cfg(not(unix))]
    let rename_result = fs::rename(&temporary.path, &paths.receipt);
    if let Err(source) = rename_result {
        remove_temporary(lock, &temporary);
        return Err(WorkspaceMaterializationError::Journal {
            path: paths.receipt.clone(),
            source,
        });
    }
    sync_publication_parent(lock, &paths.parent).map_err(|source| {
        WorkspaceMaterializationError::Journal {
            path: paths.parent.clone(),
            source,
        }
    })
}

fn validate_receipt_binding(
    receipt: &WorkspacePublicationReceipt,
    expected: GenerationIdentity,
    expected_marker: &GenerationMarkerBinding,
    destination_name: &str,
    ownership: &WorkspaceOwnershipCapability,
    context: &str,
) -> Result<(), WorkspaceMaterializationError> {
    if receipt.version != PUBLICATION_STATE_VERSION
        || !generation_binding_matches(
            receipt.generation_identity,
            receipt.ownership_marker_identity,
            &receipt.ownership_marker_nonce,
            expected,
            expected_marker.identity,
            &expected_marker.nonce,
        )
        || !authentication_tag_matches(
            &receipt.ownership_tag,
            &receipt_authentication_bytes(receipt, destination_name)?,
            ownership,
        )
    {
        return Err(WorkspaceMaterializationError::RecoveryConflict(format!(
            "{context} is not authenticated for the published filesystem generation"
        )));
    }
    Ok(())
}

fn generation_binding_matches(
    receipt_generation: GenerationIdentity,
    receipt_marker: GenerationIdentity,
    receipt_nonce: &str,
    actual_generation: GenerationIdentity,
    actual_marker: GenerationIdentity,
    actual_nonce: &str,
) -> bool {
    receipt_generation == actual_generation
        && receipt_marker == actual_marker
        && receipt_nonce == actual_nonce
}

fn validate_journal_receipt_bindings(
    journal: &PublicationJournal,
    ownership: &WorkspaceOwnershipCapability,
) -> Result<(), WorkspaceMaterializationError> {
    if journal.version != PUBLICATION_STATE_VERSION
        || !authentication_tag_matches(
            &journal.ownership_tag,
            &journal_authentication_bytes(journal)?,
            ownership,
        )
    {
        return Err(WorkspaceMaterializationError::RecoveryConflict(
            "publication journal ownership authentication failed".to_string(),
        ));
    }
    let new_identity = required_identity(journal.new_identity, "new journal generation")?;
    validate_receipt_binding(
        &journal.new_receipt,
        new_identity,
        &receipt_marker_binding(&journal.new_receipt),
        &journal.destination_name,
        ownership,
        "new journal receipt",
    )?;
    match (journal.old_identity, journal.old_receipt.as_ref()) {
        (None, None) => Ok(()),
        (Some(identity), Some(receipt)) => validate_receipt_binding(
            receipt,
            identity,
            &receipt_marker_binding(receipt),
            &journal.destination_name,
            ownership,
            "old journal receipt",
        ),
        _ => Err(WorkspaceMaterializationError::RecoveryConflict(
            "journal old generation and receipt presence disagree".to_string(),
        )),
    }
}

fn validate_active_receipt_matches(
    active: &WorkspacePublicationReceipt,
    candidates: &[(&WorkspacePublicationReceipt, GenerationIdentity)],
    destination_name: &str,
    ownership: &WorkspaceOwnershipCapability,
    context: &str,
) -> Result<(), WorkspaceMaterializationError> {
    for (candidate, identity) in candidates {
        if active == *candidate {
            return validate_receipt_binding(
                active,
                *identity,
                &receipt_marker_binding(active),
                destination_name,
                ownership,
                context,
            );
        }
    }
    Err(WorkspaceMaterializationError::RecoveryConflict(format!(
        "{context} is stale or belongs to another filesystem generation"
    )))
}

#[derive(Serialize)]
struct ReceiptAuthentication<'a> {
    domain: &'static str,
    destination_name: &'a str,
    version: u16,
    terminal_control: &'a WorkspaceExportTerminalControlV2,
    decoded_bytes: u64,
    generation_identity: GenerationIdentity,
    ownership_marker_identity: GenerationIdentity,
    ownership_marker_nonce: &'a str,
}

#[derive(Serialize)]
struct JournalAuthentication<'a> {
    domain: &'static str,
    version: u16,
    destination_name: &'a str,
    staging_name: &'a str,
    new_identity: Option<GenerationIdentity>,
    old_identity: Option<GenerationIdentity>,
    old_receipt: &'a Option<WorkspacePublicationReceipt>,
    new_receipt: &'a WorkspacePublicationReceipt,
}

fn receipt_authentication_bytes(
    receipt: &WorkspacePublicationReceipt,
    destination_name: &str,
) -> Result<Vec<u8>, WorkspaceMaterializationError> {
    serde_json::to_vec(&ReceiptAuthentication {
        domain: RECEIPT_AUTH_DOMAIN,
        destination_name,
        version: receipt.version,
        terminal_control: &receipt.terminal_control,
        decoded_bytes: receipt.decoded_bytes,
        generation_identity: receipt.generation_identity,
        ownership_marker_identity: receipt.ownership_marker_identity,
        ownership_marker_nonce: &receipt.ownership_marker_nonce,
    })
    .map_err(|error| WorkspaceMaterializationError::RecoveryConflict(error.to_string()))
}

fn journal_authentication_bytes(
    journal: &PublicationJournal,
) -> Result<Vec<u8>, WorkspaceMaterializationError> {
    serde_json::to_vec(&JournalAuthentication {
        domain: JOURNAL_AUTH_DOMAIN,
        version: journal.version,
        destination_name: &journal.destination_name,
        staging_name: &journal.staging_name,
        new_identity: journal.new_identity,
        old_identity: journal.old_identity,
        old_receipt: &journal.old_receipt,
        new_receipt: &journal.new_receipt,
    })
    .map_err(|error| WorkspaceMaterializationError::RecoveryConflict(error.to_string()))
}

fn receipt_ownership_tag(
    receipt: &WorkspacePublicationReceipt,
    destination_name: &str,
    ownership: &WorkspaceOwnershipCapability,
) -> Result<String, WorkspaceMaterializationError> {
    authentication_tag(
        &receipt_authentication_bytes(receipt, destination_name)?,
        ownership,
    )
}

fn journal_ownership_tag(
    journal: &PublicationJournal,
    ownership: &WorkspaceOwnershipCapability,
) -> Result<String, WorkspaceMaterializationError> {
    authentication_tag(&journal_authentication_bytes(journal)?, ownership)
}

fn authentication_tag(
    bytes: &[u8],
    ownership: &WorkspaceOwnershipCapability,
) -> Result<String, WorkspaceMaterializationError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(&ownership.0)
        .map_err(|error| WorkspaceMaterializationError::RecoveryConflict(error.to_string()))?;
    mac.update(bytes);
    Ok(format!("sha256:{:x}", mac.finalize().into_bytes()))
}

fn authentication_tag_matches(
    encoded: &str,
    bytes: &[u8],
    ownership: &WorkspaceOwnershipCapability,
) -> bool {
    let Some(hex) = encoded.strip_prefix("sha256:") else {
        return false;
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return false;
    }
    let mut tag = [0_u8; 32];
    for (output, pair) in tag.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let Some(high) = (pair[0] as char).to_digit(16) else {
            return false;
        };
        let Some(low) = (pair[1] as char).to_digit(16) else {
            return false;
        };
        *output = ((high << 4) | low) as u8;
    }
    Hmac::<Sha256>::new_from_slice(&ownership.0).is_ok_and(|mut mac| {
        mac.update(bytes);
        mac.verify_slice(&tag).is_ok()
    })
}

struct DurableTemporary {
    name: String,
    #[cfg_attr(unix, allow(dead_code))]
    path: PathBuf,
}

fn write_durable_temporary(
    paths: &PublicationPaths,
    lock: &WorkspacePublicationLock,
    label: &str,
    bytes: &[u8],
) -> Result<DurableTemporary, WorkspaceMaterializationError> {
    for _ in 0..16 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| WorkspaceMaterializationError::RecoveryConflict(error.to_string()))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let name = format!(".locality-{label}-{suffix}.tmp");
        let path = paths.parent.join(&name);
        #[cfg(unix)]
        let opened = rustix::fs::openat(
            lock.parent_directory(),
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map(std::fs::File::from)
        .map_err(io::Error::from);
        #[cfg(not(unix))]
        let opened = OpenOptions::new().write(true).create_new(true).open(&path);
        match opened {
            Ok(mut file) => {
                let result = file.write_all(bytes).and_then(|()| file.sync_all());
                if let Err(source) = result {
                    remove_temporary(
                        lock,
                        &DurableTemporary {
                            name,
                            path: path.clone(),
                        },
                    );
                    return Err(WorkspaceMaterializationError::Journal { path, source });
                }
                return Ok(DurableTemporary { name, path });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(WorkspaceMaterializationError::Journal { path, source });
            }
        }
    }
    Err(WorkspaceMaterializationError::RecoveryConflict(
        "could not allocate durable publication state".to_string(),
    ))
}

fn remove_temporary(lock: &WorkspacePublicationLock, temporary: &DurableTemporary) {
    #[cfg(unix)]
    let _ = rustix::fs::unlinkat(lock.parent_directory(), &temporary.name, AtFlags::empty());
    #[cfg(not(unix))]
    let _ = fs::remove_file(&temporary.path);
}

fn publication_entry_exists(
    lock: &WorkspacePublicationLock,
    _paths: &PublicationPaths,
    name: &str,
    path: &Path,
) -> Result<bool, WorkspaceMaterializationError> {
    #[cfg(unix)]
    let result = rustix::fs::statat(lock.parent_directory(), name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|_| true)
        .or_else(|error| {
            if error == rustix::io::Errno::NOENT {
                Ok(false)
            } else {
                Err(io::Error::from(error))
            }
        });
    #[cfg(not(unix))]
    let result = match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    };
    result.map_err(|source| WorkspaceMaterializationError::Journal {
        path: path.to_path_buf(),
        source,
    })
}

fn read_json_locked<T: for<'de> Deserialize<'de>>(
    lock: &WorkspacePublicationLock,
    _paths: &PublicationPaths,
    name: &str,
    path: &Path,
) -> Result<T, WorkspaceMaterializationError> {
    #[cfg(unix)]
    let bytes = rustix::fs::openat(
        lock.parent_directory(),
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(std::fs::File::from)
    .map_err(io::Error::from)
    .and_then(|mut file| {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    });
    #[cfg(not(unix))]
    let bytes = fs::read(path);
    let bytes = bytes.map_err(|source| WorkspaceMaterializationError::Journal {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        WorkspaceMaterializationError::RecoveryConflict(format!(
            "invalid publication state `{}`: {error}",
            path.display()
        ))
    })
}

fn remove_journal_if_present(
    paths: &PublicationPaths,
    lock: &WorkspacePublicationLock,
) -> Result<(), WorkspaceMaterializationError> {
    #[cfg(unix)]
    let result = rustix::fs::unlinkat(
        lock.parent_directory(),
        &paths.journal_name,
        AtFlags::empty(),
    )
    .map_err(io::Error::from);
    #[cfg(not(unix))]
    let result = fs::remove_file(&paths.journal);
    match result {
        Ok(()) => sync_publication_parent(lock, &paths.parent).map_err(|source| {
            WorkspaceMaterializationError::Journal {
                path: paths.parent.clone(),
                source,
            }
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(WorkspaceMaterializationError::Journal {
            path: paths.journal.clone(),
            source,
        }),
    }
}

fn sync_publication_parent(lock: &WorkspacePublicationLock, _path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        rustix::fs::fsync(lock.parent_directory()).map_err(Into::into)
    }
    #[cfg(not(unix))]
    {
        match fs::File::open(_path).and_then(|directory| directory.sync_all()) {
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
}

fn path_file_name(path: &Path) -> Result<String, WorkspaceMaterializationError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .ok_or(WorkspaceMaterializationError::InvalidDestination)
}

fn generation_identity_if_exists_locked(
    lock: &WorkspacePublicationLock,
    paths: &PublicationPaths,
    name: &str,
) -> Result<Option<GenerationIdentity>, WorkspaceMaterializationError> {
    #[cfg(unix)]
    let result = workspace_generation_identity_if_exists_at(
        lock.parent_directory(),
        std::ffi::OsStr::new(name),
    );
    #[cfg(not(unix))]
    let result = workspace_generation_identity_if_exists(&paths.parent.join(name));
    result
        .map(|identity| identity.map(Into::into))
        .map_err(|source| WorkspaceMaterializationError::Journal {
            path: paths.parent.join(name),
            source,
        })
}

fn generation_identity_locked(
    lock: &WorkspacePublicationLock,
    paths: &PublicationPaths,
    name: &str,
) -> Result<Option<GenerationIdentity>, WorkspaceMaterializationError> {
    generation_identity_if_exists_locked(lock, paths, name)?
        .ok_or_else(|| {
            WorkspaceMaterializationError::RecoveryConflict(format!(
                "publication path `{}` disappeared",
                paths.parent.join(name).display()
            ))
        })
        .map(Some)
}

fn generation_marker_binding_locked(
    lock: &WorkspacePublicationLock,
    paths: &PublicationPaths,
    name: &str,
) -> Result<GenerationMarkerBinding, WorkspaceMaterializationError> {
    #[cfg(unix)]
    let result = workspace_generation_file_binding_at(
        lock.parent_directory(),
        std::ffi::OsStr::new(name),
        WORKSPACE_OWNERSHIP_MARKER,
        WORKSPACE_OWNERSHIP_NONCE_BYTES,
    );
    #[cfg(not(unix))]
    let result = workspace_generation_file_binding(
        &paths.parent.join(name),
        WORKSPACE_OWNERSHIP_MARKER,
        WORKSPACE_OWNERSHIP_NONCE_BYTES,
    );
    result
        .map_err(|source| WorkspaceMaterializationError::Journal {
            path: paths.parent.join(name).join(WORKSPACE_OWNERSHIP_MARKER),
            source,
        })
        .and_then(marker_binding_from_file)
}

fn staging_marker_binding(
    staging: &StagingDirectory,
) -> Result<GenerationMarkerBinding, WorkspaceMaterializationError> {
    marker_binding_from_file(
        staging.file_binding(WORKSPACE_OWNERSHIP_MARKER, WORKSPACE_OWNERSHIP_NONCE_BYTES)?,
    )
}

fn marker_binding_from_file(
    marker: WorkspaceGenerationFileBinding,
) -> Result<GenerationMarkerBinding, WorkspaceMaterializationError> {
    if marker.content.len() != WORKSPACE_OWNERSHIP_NONCE_BYTES {
        return Err(WorkspaceMaterializationError::RecoveryConflict(
            "workspace ownership marker has an invalid nonce length".to_string(),
        ));
    }
    Ok(GenerationMarkerBinding {
        identity: marker.identity.into(),
        nonce: marker
            .content
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    })
}

fn receipt_marker_binding(receipt: &WorkspacePublicationReceipt) -> GenerationMarkerBinding {
    GenerationMarkerBinding {
        identity: receipt.ownership_marker_identity,
        nonce: receipt.ownership_marker_nonce.clone(),
    }
}

fn marker_nonce_bytes(encoded: &str) -> Result<[u8; 32], WorkspaceMaterializationError> {
    if encoded.len() != WORKSPACE_OWNERSHIP_NONCE_BYTES * 2
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(WorkspaceMaterializationError::RecoveryConflict(
            "workspace ownership marker nonce is not canonical lower hex".to_string(),
        ));
    }
    let mut nonce = [0_u8; WORKSPACE_OWNERSHIP_NONCE_BYTES];
    for (output, pair) in nonce.iter_mut().zip(encoded.as_bytes().chunks_exact(2)) {
        let high = (pair[0] as char).to_digit(16).expect("validated hex");
        let low = (pair[1] as char).to_digit(16).expect("validated hex");
        *output = ((high << 4) | low) as u8;
    }
    Ok(nonce)
}

fn identity_matches(
    actual: Option<GenerationIdentity>,
    expected: Option<GenerationIdentity>,
) -> bool {
    match expected {
        Some(expected) => actual == Some(expected),
        None => false,
    }
}

fn required_identity(
    identity: Option<GenerationIdentity>,
    label: &str,
) -> Result<GenerationIdentity, WorkspaceMaterializationError> {
    identity.ok_or_else(|| {
        WorkspaceMaterializationError::RecoveryConflict(format!(
            "durable journal is missing {label} identity"
        ))
    })
}

fn remove_generation_locked(
    lock: &WorkspacePublicationLock,
    paths: &PublicationPaths,
    name: &str,
    expected: GenerationIdentity,
    receipt: &WorkspacePublicationReceipt,
) -> Result<(), WorkspaceMaterializationError> {
    let marker_content = marker_nonce_bytes(&receipt.ownership_marker_nonce)?;
    #[cfg(unix)]
    let result = remove_workspace_generation_at(
        lock.parent_directory(),
        std::ffi::OsStr::new(name),
        expected.into(),
        WORKSPACE_OWNERSHIP_MARKER,
        receipt.ownership_marker_identity.into(),
        &marker_content,
    );
    #[cfg(not(unix))]
    let result = remove_workspace_generation(
        &paths.parent.join(name),
        expected.into(),
        WORKSPACE_OWNERSHIP_MARKER,
        receipt.ownership_marker_identity.into(),
        &marker_content,
    );
    result.map_err(|source| WorkspaceMaterializationError::Journal {
        path: paths.parent.join(name),
        source,
    })
}

fn repair_generation_locked(
    lock: &WorkspacePublicationLock,
    paths: &PublicationPaths,
    name: &str,
    expected: GenerationIdentity,
) -> Result<(), WorkspaceMaterializationError> {
    #[cfg(unix)]
    let result = repair_workspace_generation_at(
        lock.parent_directory(),
        std::ffi::OsStr::new(name),
        expected.into(),
    );
    #[cfg(not(unix))]
    let result = repair_workspace_generation(&paths.parent.join(name), expected.into());
    result.map_err(|source| WorkspaceMaterializationError::Journal {
        path: paths.parent.join(name),
        source,
    })
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
    stage_workspace_archive_locked(archive, destination, limits, session, offer, None)
}

fn stage_workspace_archive_locked<Body: Read>(
    archive: ReplicaArchive<Body>,
    destination: &Path,
    limits: WorkspaceMaterializationLimits,
    session: &WorkspaceProfileSessionV2,
    offer: &WorkspaceExportOfferV2,
    lock: Option<&WorkspacePublicationLock>,
) -> Result<StagedWorkspaceMaterialization, WorkspaceMaterializationError> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(WorkspaceMaterializationError::InvalidDestination)?;
    if destination.file_name().is_none() {
        return Err(WorkspaceMaterializationError::InvalidDestination);
    }
    if lock.is_none() {
        match fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) | Err(_) => {
                return Err(WorkspaceMaterializationError::DestinationParentMissing(
                    parent.to_path_buf(),
                ));
            }
        }
    }

    let staging = match lock {
        Some(lock) => StagingDirectory::create_for_publication(parent, lock)?,
        None => StagingDirectory::create(parent)?,
    };
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
    let mut marker = [0_u8; 32];
    getrandom::fill(&mut marker)
        .map_err(|error| WorkspaceMaterializationError::RecoveryConflict(error.to_string()))?;
    let mut marker_body = marker.as_slice();
    staging.write_file(
        WORKSPACE_OWNERSHIP_MARKER,
        &mut marker_body,
        marker.len() as u64,
    )?;
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

#[cfg(test)]
mod ownership_tests {
    use super::{GenerationIdentity, generation_binding_matches};

    #[test]
    fn reused_inode_or_file_id_does_not_reuse_generation_ownership() {
        let reused_root = GenerationIdentity {
            device: 7,
            inode: 42,
            inode_high: 0,
        };
        let old_marker = GenerationIdentity {
            device: 7,
            inode: 100,
            inode_high: 0,
        };
        let replacement_marker = GenerationIdentity {
            device: 7,
            inode: 101,
            inode_high: 0,
        };

        assert!(!generation_binding_matches(
            reused_root,
            old_marker,
            "old-nonce",
            reused_root,
            replacement_marker,
            "old-nonce",
        ));
        assert!(!generation_binding_matches(
            reused_root,
            old_marker,
            "old-nonce",
            reused_root,
            old_marker,
            "replacement-nonce",
        ));
    }
}
