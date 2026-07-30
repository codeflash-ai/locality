//! Staged, durable publication of validated generation-2 workspace archives.

use std::fmt::{Display, Formatter};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use locality_protocol::workspace_api_v2::{WorkspaceExportOfferV2, WorkspaceProfileSessionV2};
use locality_protocol::workspace_export_v2::WorkspaceExportTerminalControlV2;
use serde::{Deserialize, Serialize};

use crate::remote_truth::{ReplicaArchive, ReplicaArchiveEncoding};
use crate::replica_materializer::{
    ReplicaMaterializationError, StagingDirectory, WorkspaceGenerationIdentity,
    remove_workspace_generation, repair_workspace_generation,
    workspace_generation_identity_if_exists,
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
        let expected_staging = self.staging.identity()?;
        self.publish_initial_expected(destination, expected_staging)
    }

    fn publish_initial_expected(
        mut self,
        destination: &Path,
        expected_staging: WorkspaceGenerationIdentity,
    ) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
        if fs::symlink_metadata(destination).is_ok() {
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
        let expected_staging = self.staging.identity()?;
        let expected_destination = generation_identity(destination)?
            .ok_or_else(|| WorkspaceMaterializationError::DestinationExists(destination.into()))?
            .into();
        self.publish_exchange_expected(destination, expected_staging, expected_destination)
    }

    fn publish_exchange_expected(
        mut self,
        destination: &Path,
        expected_staging: WorkspaceGenerationIdentity,
        expected_destination: WorkspaceGenerationIdentity,
    ) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationIdentity {
    device: u64,
    inode: u64,
    #[serde(default)]
    inode_high: u64,
}

const PUBLICATION_STATE_VERSION: u16 = 2;

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
) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
    recover_workspace_publication(destination)?;
    let staged = stage_workspace_archive(archive, destination, limits, session, offer)?;
    publish_staged_workspace(staged, destination)
}

pub fn publish_staged_workspace(
    staged: StagedWorkspaceMaterialization,
    destination: &Path,
) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
    publish_staged_workspace_with_hooks(staged, destination, &mut ())
}

pub fn publish_staged_workspace_with_hooks<H: WorkspacePublicationHooks>(
    staged: StagedWorkspaceMaterialization,
    destination: &Path,
    hooks: &mut H,
) -> Result<PublishedWorkspace, WorkspaceMaterializationError> {
    let paths = PublicationPaths::new(destination)?;
    if paths.journal.exists() {
        return Err(WorkspaceMaterializationError::RecoveryRequired(
            paths.journal,
        ));
    }
    let destination_exists = ordinary_directory_exists(destination)?;
    let new_identity: GenerationIdentity = staged.staging.identity()?.into();
    let old_identity = if destination_exists {
        generation_identity(destination)?
    } else {
        None
    };
    let old_receipt = if let Some(identity) = old_identity {
        let receipt = load_workspace_publication_receipt(destination)?.ok_or_else(|| {
            WorkspaceMaterializationError::RecoveryConflict(
                "an existing workspace root has no durable active receipt".to_string(),
            )
        })?;
        validate_receipt_binding(&receipt, identity, "active workspace receipt")?;
        Some(receipt)
    } else {
        None
    };
    let staging_name = path_file_name(staged.staging.path())?;
    let new_receipt = WorkspacePublicationReceipt {
        version: PUBLICATION_STATE_VERSION,
        terminal_control: staged.validated.terminal_control.clone(),
        decoded_bytes: staged.decoded_bytes,
        generation_identity: new_identity,
    };
    let journal = PublicationJournal {
        version: PUBLICATION_STATE_VERSION,
        destination_name: path_file_name(destination)?,
        staging_name,
        new_identity: Some(new_identity),
        old_identity,
        old_receipt,
        new_receipt,
    };
    validate_journal_receipt_bindings(&journal)?;
    create_durable_journal(&paths, &journal)?;
    if let Err(source) = hooks.checkpoint(WorkspacePublicationCheckpoint::JournalDurable) {
        return Err(WorkspaceMaterializationError::Journal {
            path: paths.journal,
            source,
        });
    }

    let publication = if destination_exists {
        validate_journal_receipt_bindings(&journal)?;
        staged.publish_exchange_expected(
            destination,
            required_identity(journal.new_identity, "new journal generation")?.into(),
            required_identity(journal.old_identity, "old journal generation")?.into(),
        )
    } else {
        staged.publish_initial_expected(
            destination,
            required_identity(journal.new_identity, "new journal generation")?.into(),
        )
    };
    let published = match publication {
        Ok(published) => published,
        Err(error) => {
            // Publication primitives can fail after an exchange has committed
            // (for example while syncing the parent). Let identity-based
            // recovery distinguish that state from a pre-exchange failure.
            recover_workspace_publication(destination)?;
            return Err(error);
        }
    };
    if let Err(source) = hooks.checkpoint(WorkspacePublicationCheckpoint::PublicationComplete) {
        return Err(WorkspaceMaterializationError::Journal {
            path: paths.journal,
            source,
        });
    }
    replace_durable_receipt(&paths, destination, &journal.new_receipt)?;
    if let Err(source) = hooks.checkpoint(WorkspacePublicationCheckpoint::ReceiptDurable) {
        return Err(WorkspaceMaterializationError::Journal {
            path: paths.journal,
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
            "old-generation cleanup receipt",
        )?;
        remove_workspace_generation(
            old_generation,
            required_identity(journal.old_identity, "old journal generation")?.into(),
        )
        .map_err(|source| WorkspaceMaterializationError::Journal {
            path: old_generation.clone(),
            source,
        })?;
    }
    if let Err(source) = hooks.checkpoint(WorkspacePublicationCheckpoint::CleanupComplete) {
        return Err(WorkspaceMaterializationError::Journal {
            path: paths.journal,
            source,
        });
    }
    remove_journal_if_present(&paths)?;
    Ok(published)
}

/// Recover an interrupted initial publication or refresh using the durable
/// journal plus filesystem identities. It either keeps the old complete root
/// or completes the new receipt; it never merges generations in place.
pub fn recover_workspace_publication(
    destination: &Path,
) -> Result<(), WorkspaceMaterializationError> {
    let paths = PublicationPaths::new(destination)?;
    if !paths.journal.exists() {
        return Ok(());
    }
    let journal: PublicationJournal = read_json(&paths.journal)?;
    if journal.version != PUBLICATION_STATE_VERSION
        || journal.destination_name != path_file_name(destination)?
    {
        return Err(WorkspaceMaterializationError::RecoveryConflict(
            "journal version or destination binding is invalid".to_string(),
        ));
    }
    validate_journal_receipt_bindings(&journal)?;
    if !journal.staging_name.starts_with(".locality-stage-")
        || journal.staging_name.contains(['/', '\\'])
    {
        return Err(WorkspaceMaterializationError::RecoveryConflict(
            "journal staging name is invalid".to_string(),
        ));
    }
    let staging = paths.parent.join(&journal.staging_name);
    let destination_identity = generation_identity_if_exists(destination)?;
    let staging_identity = generation_identity_if_exists(&staging)?;
    let active_receipt = load_workspace_publication_receipt(destination)?;

    if journal.old_identity.is_none() {
        if identity_matches(destination_identity, journal.new_identity) {
            if let Some(receipt) = active_receipt.as_ref() {
                validate_active_receipt_matches(
                    receipt,
                    &[(
                        &journal.new_receipt,
                        journal.new_receipt.generation_identity,
                    )],
                    "initial publication receipt",
                )?;
            }
            repair_generation(
                destination,
                required_identity(journal.new_identity, "new generation")?,
            )?;
            replace_durable_receipt(&paths, destination, &journal.new_receipt)?;
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
            remove_generation(
                &staging,
                required_identity(journal.new_identity, "new generation")?,
            )?;
        } else if destination_identity.is_some()
            || staging_identity.is_some()
            || active_receipt.is_some()
        {
            return Err(WorkspaceMaterializationError::RecoveryConflict(
                "initial publication paths do not match the durable journal".to_string(),
            ));
        }
        remove_journal_if_present(&paths)?;
        return Ok(());
    }

    if identity_matches(destination_identity, journal.new_identity)
        && identity_matches(staging_identity, journal.old_identity)
    {
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
            "exchanged workspace receipt",
        )?;
        repair_generation(
            destination,
            required_identity(journal.new_identity, "new generation")?,
        )?;
        replace_durable_receipt(&paths, destination, &journal.new_receipt)?;
        validate_receipt_binding(
            journal.old_receipt.as_ref().expect("validated old receipt"),
            required_identity(journal.old_identity, "old generation")?,
            "recovery cleanup receipt",
        )?;
        remove_generation(
            &staging,
            required_identity(journal.old_identity, "old generation")?,
        )?;
        remove_journal_if_present(&paths)?;
        return Ok(());
    }
    if identity_matches(destination_identity, journal.old_identity)
        && identity_matches(staging_identity, journal.new_identity)
    {
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
            "pre-exchange workspace receipt",
        )?;
        repair_generation(
            destination,
            required_identity(journal.old_identity, "old generation")?,
        )?;
        remove_generation(
            &staging,
            required_identity(journal.new_identity, "new generation")?,
        )?;
        remove_journal_if_present(&paths)?;
        return Ok(());
    }
    if identity_matches(destination_identity, journal.old_identity) && staging_identity.is_none() {
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
            "rolled-back workspace receipt",
        )?;
        repair_generation(
            destination,
            required_identity(journal.old_identity, "old generation")?,
        )?;
        remove_journal_if_present(&paths)?;
        return Ok(());
    }
    if identity_matches(destination_identity, journal.new_identity) && staging_identity.is_none() {
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
            "post-cleanup workspace receipt",
        )?;
        repair_generation(
            destination,
            required_identity(journal.new_identity, "new generation")?,
        )?;
        remove_journal_if_present(&paths)?;
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
    if !paths.receipt.exists() {
        return Ok(None);
    }
    read_json(&paths.receipt).map(Some)
}

/// Recovers any local publication journal and confirms that an existing root
/// is the exact filesystem generation named by its durable V2 receipt.
pub fn recover_and_verify_workspace_publication_state(
    destination: &Path,
) -> Result<bool, WorkspaceMaterializationError> {
    let paths = PublicationPaths::new(destination)?;
    if paths.journal.exists() {
        recover_workspace_publication(destination)?;
    }
    let Some(identity) = generation_identity_if_exists(destination)? else {
        if paths.receipt.exists() {
            return Err(WorkspaceMaterializationError::RecoveryConflict(
                "workspace receipt exists without its published generation".to_string(),
            ));
        }
        return Ok(false);
    };
    let Some(receipt) = load_workspace_publication_receipt(destination)? else {
        return Ok(false);
    };
    validate_receipt_binding(&receipt, identity, "active workspace receipt")?;
    Ok(true)
}

struct PublicationPaths {
    parent: PathBuf,
    journal: PathBuf,
    receipt: PathBuf,
}

impl PublicationPaths {
    fn new(destination: &Path) -> Result<Self, WorkspaceMaterializationError> {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or(WorkspaceMaterializationError::InvalidDestination)?
            .to_path_buf();
        let destination_name = path_file_name(destination)?;
        Ok(Self {
            journal: parent.join(format!(".locality-{destination_name}.publication.json")),
            receipt: parent.join(format!(".locality-{destination_name}.receipt.json")),
            parent,
        })
    }
}

fn create_durable_journal(
    paths: &PublicationPaths,
    journal: &PublicationJournal,
) -> Result<(), WorkspaceMaterializationError> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|error| WorkspaceMaterializationError::RecoveryConflict(error.to_string()))?;
    let temporary = write_durable_temporary(paths, "journal", &bytes)?;
    let link_result = fs::hard_link(&temporary, &paths.journal);
    let _ = fs::remove_file(&temporary);
    link_result.map_err(|source| WorkspaceMaterializationError::Journal {
        path: paths.journal.clone(),
        source,
    })?;
    sync_parent_directory(&paths.parent).map_err(|source| WorkspaceMaterializationError::Journal {
        path: paths.parent.clone(),
        source,
    })
}

fn replace_durable_receipt(
    paths: &PublicationPaths,
    destination: &Path,
    receipt: &WorkspacePublicationReceipt,
) -> Result<(), WorkspaceMaterializationError> {
    let identity = generation_identity_if_exists(destination)?.ok_or_else(|| {
        WorkspaceMaterializationError::RecoveryConflict(
            "cannot publish a receipt without its workspace generation".to_string(),
        )
    })?;
    validate_receipt_binding(receipt, identity, "durable workspace receipt")?;
    let bytes = serde_json::to_vec(receipt)
        .map_err(|error| WorkspaceMaterializationError::RecoveryConflict(error.to_string()))?;
    let temporary = write_durable_temporary(paths, "receipt", &bytes)?;
    if let Err(source) = fs::rename(&temporary, &paths.receipt) {
        let _ = fs::remove_file(&temporary);
        return Err(WorkspaceMaterializationError::Journal {
            path: paths.receipt.clone(),
            source,
        });
    }
    sync_parent_directory(&paths.parent).map_err(|source| WorkspaceMaterializationError::Journal {
        path: paths.parent.clone(),
        source,
    })
}

fn validate_receipt_binding(
    receipt: &WorkspacePublicationReceipt,
    expected: GenerationIdentity,
    context: &str,
) -> Result<(), WorkspaceMaterializationError> {
    if receipt.version != PUBLICATION_STATE_VERSION || receipt.generation_identity != expected {
        return Err(WorkspaceMaterializationError::RecoveryConflict(format!(
            "{context} does not match the published filesystem generation"
        )));
    }
    Ok(())
}

fn validate_journal_receipt_bindings(
    journal: &PublicationJournal,
) -> Result<(), WorkspaceMaterializationError> {
    let new_identity = required_identity(journal.new_identity, "new journal generation")?;
    validate_receipt_binding(&journal.new_receipt, new_identity, "new journal receipt")?;
    match (journal.old_identity, journal.old_receipt.as_ref()) {
        (None, None) => Ok(()),
        (Some(identity), Some(receipt)) => {
            validate_receipt_binding(receipt, identity, "old journal receipt")
        }
        _ => Err(WorkspaceMaterializationError::RecoveryConflict(
            "journal old generation and receipt presence disagree".to_string(),
        )),
    }
}

fn validate_active_receipt_matches(
    active: &WorkspacePublicationReceipt,
    candidates: &[(&WorkspacePublicationReceipt, GenerationIdentity)],
    context: &str,
) -> Result<(), WorkspaceMaterializationError> {
    for (candidate, identity) in candidates {
        if active == *candidate {
            return validate_receipt_binding(active, *identity, context);
        }
    }
    Err(WorkspaceMaterializationError::RecoveryConflict(format!(
        "{context} is stale or belongs to another filesystem generation"
    )))
}

fn write_durable_temporary(
    paths: &PublicationPaths,
    label: &str,
    bytes: &[u8],
) -> Result<PathBuf, WorkspaceMaterializationError> {
    for _ in 0..16 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| WorkspaceMaterializationError::RecoveryConflict(error.to_string()))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = paths.parent.join(format!(".locality-{label}-{suffix}.tmp"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let result = file.write_all(bytes).and_then(|()| file.sync_all());
                if let Err(source) = result {
                    let _ = fs::remove_file(&path);
                    return Err(WorkspaceMaterializationError::Journal { path, source });
                }
                return Ok(path);
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

fn read_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
) -> Result<T, WorkspaceMaterializationError> {
    let bytes = fs::read(path).map_err(|source| WorkspaceMaterializationError::Journal {
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
) -> Result<(), WorkspaceMaterializationError> {
    match fs::remove_file(&paths.journal) {
        Ok(()) => sync_parent_directory(&paths.parent).map_err(|source| {
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

fn sync_parent_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
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
}

fn path_file_name(path: &Path) -> Result<String, WorkspaceMaterializationError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .ok_or(WorkspaceMaterializationError::InvalidDestination)
}

fn ordinary_directory_exists(path: &Path) -> Result<bool, WorkspaceMaterializationError> {
    workspace_generation_identity_if_exists(path)
        .map(|identity| identity.is_some())
        .map_err(|source| WorkspaceMaterializationError::Journal {
            path: path.to_path_buf(),
            source,
        })
}

fn generation_identity_if_exists(
    path: &Path,
) -> Result<Option<GenerationIdentity>, WorkspaceMaterializationError> {
    workspace_generation_identity_if_exists(path)
        .map(|identity| identity.map(Into::into))
        .map_err(|source| WorkspaceMaterializationError::Journal {
            path: path.to_path_buf(),
            source,
        })
}

fn generation_identity(
    path: &Path,
) -> Result<Option<GenerationIdentity>, WorkspaceMaterializationError> {
    generation_identity_if_exists(path)?
        .ok_or_else(|| {
            WorkspaceMaterializationError::RecoveryConflict(format!(
                "publication path `{}` disappeared",
                path.display()
            ))
        })
        .map(Some)
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

fn remove_generation(
    path: &Path,
    expected: GenerationIdentity,
) -> Result<(), WorkspaceMaterializationError> {
    remove_workspace_generation(path, expected.into()).map_err(|source| {
        WorkspaceMaterializationError::Journal {
            path: path.to_path_buf(),
            source,
        }
    })
}

fn repair_generation(
    path: &Path,
    expected: GenerationIdentity,
) -> Result<(), WorkspaceMaterializationError> {
    repair_workspace_generation(path, expected.into()).map_err(|source| {
        WorkspaceMaterializationError::Journal {
            path: path.to_path_buf(),
            source,
        }
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
