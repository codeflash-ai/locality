//! Portable freshness delivery contracts.
//!
//! These values deliberately contain no endpoint, credential, tenant route,
//! database handle, or host path. Transports authenticate a delta before
//! handing it to local delivery code; the terminal receipt then binds the
//! exact authorized metadata that the local client validates and persists.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};

use locality_core::portable::{
    ContentVersionId, LogicalPath, ProjectionId, SourceConnectionId, SourceGenerationId,
};
use locality_core::workspace_layout::PortableMountId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::FreshnessEpoch;
use crate::workspace_layout::LayoutDigest;

pub const FRESHNESS_DELIVERY_READER_VERSION: u16 = 1;
pub const GENERATION_DELTA_FORMAT_VERSION: u16 = 1;
pub const MAX_GENERATION_DELTA_ENTRIES: usize = 100_000;
pub const MAX_GENERATION_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_GENERATION_DELTA_CONTENT_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_DELIVERY_ID_BYTES: usize = 128;
pub const MAX_DELIVERY_TIMESTAMP_BYTES: usize = 64;
pub const MAX_RETRY_AFTER_SECONDS: u64 = 24 * 60 * 60;

pub const FRESHNESS_HEALTH_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/freshness-health-v1.json");
pub const GENERATION_DELTA_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-delta-v1.json");
pub const GENERATION_DELTA_RECEIPT_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-delta-receipt-v1.json");
pub const GENERATION_DELTA_PREIMAGE_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-delta-preimage-v1.json");

/// Stable, redaction-safe explanation shared by API, CLI, Desktop, and logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessReasonCode {
    ProviderAuthenticationRequired,
    ProviderUnavailable,
    ProviderCooldown,
    RefreshQueued,
    RefreshProcessing,
    RefreshApplying,
    RepairRequired,
    GenerationIncomplete,
    GenerationUnavailable,
    LocalTreeBehind,
    LocalChangesPending,
    MergeConflict,
    UpdateRequired,
    #[serde(other)]
    Unknown,
}

/// Bounded retry behavior. Human-readable provider errors are intentionally
/// absent; callers map this class and [`FreshnessReasonCode`] to local copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessRetryClass {
    Never,
    Immediate,
    AfterDelay,
    AfterRefresh,
    AfterUserAction,
    AfterUpdate,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessRetry {
    pub class: FreshnessRetryClass,
    pub retry_after_seconds: Option<u64>,
}

impl FreshnessRetry {
    pub fn validate(&self) -> Result<(), FreshnessDeliveryError> {
        match (self.class, self.retry_after_seconds) {
            (FreshnessRetryClass::Unknown, _) => Err(FreshnessDeliveryError::UnknownRetryClass),
            (FreshnessRetryClass::AfterDelay, Some(seconds))
                if (1..=MAX_RETRY_AFTER_SECONDS).contains(&seconds) =>
            {
                Ok(())
            }
            (FreshnessRetryClass::AfterDelay, _) => Err(FreshnessDeliveryError::InvalidRetryDelay),
            (_, None) => Ok(()),
            (_, Some(_)) => Err(FreshnessDeliveryError::UnexpectedRetryDelay),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealthState {
    Healthy,
    Degraded,
    Unavailable,
    AuthenticationRequired,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderWorkerProgress {
    Idle,
    Queued,
    Fetching,
    Publishing,
    #[serde(other)]
    Unknown,
}

/// Current provider/control-plane health. This does not claim that a retained
/// generation is unreadable or that a local tree is current.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub source_connection_id: SourceConnectionId,
    pub state: ProviderHealthState,
    pub reason: Option<FreshnessReasonCode>,
    pub retry: Option<FreshnessRetry>,
    pub epochs: crate::ScopeFreshnessEpochs,
    pub worker_progress: ProviderWorkerProgress,
    pub latest_observation_at: Option<String>,
    pub provider_cooldown_until: Option<String>,
}

impl ProviderHealth {
    pub fn validate(&self) -> Result<(), FreshnessDeliveryError> {
        validate_identifier("source_connection_id", self.source_connection_id.as_str())?;
        self.epochs
            .validate()
            .map_err(|_| FreshnessDeliveryError::InvalidEpochOrder)?;
        if self.state == ProviderHealthState::Unknown {
            return Err(FreshnessDeliveryError::UnknownHealthState);
        }
        if self.worker_progress == ProviderWorkerProgress::Unknown {
            return Err(FreshnessDeliveryError::UnknownWorkerProgress);
        }
        validate_reason(self.reason)?;
        if let Some(retry) = self.retry {
            retry.validate()?;
        }
        validate_optional_timestamp(&self.latest_observation_at)?;
        validate_optional_timestamp(&self.provider_cooldown_until)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationGenerationState {
    Complete,
    Incomplete,
    Unavailable,
    #[serde(other)]
    Unknown,
}

/// Health of one immutable publication generation. Provider and local-delivery
/// health remain independent of these facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicationGenerationHealth {
    pub source_connection_id: SourceConnectionId,
    pub generation_id: SourceGenerationId,
    pub state: PublicationGenerationState,
    pub verified: bool,
    pub retained: bool,
    pub selectable: bool,
    pub applied_receipt_sha256: Option<String>,
    pub reason: Option<FreshnessReasonCode>,
    pub retry: Option<FreshnessRetry>,
}

impl PublicationGenerationHealth {
    pub fn validate(&self) -> Result<(), FreshnessDeliveryError> {
        validate_identifier("source_connection_id", self.source_connection_id.as_str())?;
        validate_identifier("generation_id", self.generation_id.as_str())?;
        if self.state == PublicationGenerationState::Unknown {
            return Err(FreshnessDeliveryError::UnknownHealthState);
        }
        validate_reason(self.reason)?;
        if let Some(retry) = self.retry {
            retry.validate()?;
        }
        if let Some(digest) = &self.applied_receipt_sha256 {
            validate_sha256(digest)?;
        }
        if self.selectable
            && (self.state != PublicationGenerationState::Complete
                || !self.verified
                || !self.retained
                || self.applied_receipt_sha256.is_none())
        {
            return Err(FreshnessDeliveryError::UnselectableGenerationFacts);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveredTreeState {
    Current,
    Behind,
    Dirty,
    Conflicted,
    UpdateRequired,
    #[serde(other)]
    Unknown,
}

/// Machine-local delivery state. It never asserts provider or publication
/// health merely because the delivered tree is clean.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredTreeHealth {
    pub mount_id: PortableMountId,
    pub state: DeliveredTreeState,
    pub observed_generation_id: Option<SourceGenerationId>,
    pub available_generation_id: Option<SourceGenerationId>,
    pub clean_path_count: u64,
    pub dirty_path_count: u64,
    pub pending_path_count: u64,
    pub conflicted_path_count: u64,
    pub last_delta_receipt_sha256: Option<String>,
    pub reason: Option<FreshnessReasonCode>,
    pub retry: Option<FreshnessRetry>,
}

impl DeliveredTreeHealth {
    pub fn validate(&self) -> Result<(), FreshnessDeliveryError> {
        if self.state == DeliveredTreeState::Unknown {
            return Err(FreshnessDeliveryError::UnknownHealthState);
        }
        validate_reason(self.reason)?;
        if let Some(retry) = self.retry {
            retry.validate()?;
        }
        if let Some(generation) = &self.observed_generation_id {
            validate_identifier("observed_generation_id", generation.as_str())?;
        }
        if let Some(generation) = &self.available_generation_id {
            validate_identifier("available_generation_id", generation.as_str())?;
        }
        if let Some(digest) = &self.last_delta_receipt_sha256 {
            validate_sha256(digest)?;
        }
        Ok(())
    }
}

/// The three deliberately independent health layers returned together when a
/// caller needs one snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessHealth {
    pub provider: ProviderHealth,
    pub publication: PublicationGenerationHealth,
    pub local_delivery: DeliveredTreeHealth,
}

impl FreshnessHealth {
    pub fn validate(&self) -> Result<(), FreshnessDeliveryError> {
        self.provider.validate()?;
        self.publication.validate()?;
        self.local_delivery.validate()?;
        if self.provider.source_connection_id != self.publication.source_connection_id {
            return Err(FreshnessDeliveryError::HealthSourceMismatch);
        }
        Ok(())
    }
}

/// Stable old/new identity for one projected file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationFileIdentity {
    pub projection_id: ProjectionId,
    pub logical_path: LogicalPath,
    pub content_version_id: ContentVersionId,
    pub content_sha256: String,
    pub byte_length: u64,
}

impl GenerationFileIdentity {
    pub fn validate(&self) -> Result<(), FreshnessDeliveryError> {
        validate_identifier("projection_id", self.projection_id.as_str())?;
        validate_identifier("content_version_id", self.content_version_id.as_str())?;
        validate_sha256(&self.content_sha256)?;
        if self.byte_length > MAX_GENERATION_FILE_BYTES {
            return Err(FreshnessDeliveryError::FileContentTooLarge {
                actual: self.byte_length,
            });
        }
        Ok(())
    }
}

/// One create, update, rename, or deletion. Content bytes are obtained through
/// the authenticated delivery transport and verified against `new`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationDeltaEntry {
    pub old: Option<GenerationFileIdentity>,
    pub new: Option<GenerationFileIdentity>,
}

impl GenerationDeltaEntry {
    pub fn projection_id(&self) -> Option<&ProjectionId> {
        self.new
            .as_ref()
            .map(|identity| &identity.projection_id)
            .or_else(|| self.old.as_ref().map(|identity| &identity.projection_id))
    }

    pub fn validate(&self) -> Result<(), FreshnessDeliveryError> {
        if self.old.is_none() && self.new.is_none() {
            return Err(FreshnessDeliveryError::EmptyDeltaEntry);
        }
        if let Some(old) = &self.old {
            old.validate()?;
        }
        if let Some(new) = &self.new {
            new.validate()?;
        }
        if let (Some(old), Some(new)) = (&self.old, &self.new) {
            if old.projection_id != new.projection_id {
                return Err(FreshnessDeliveryError::ProjectionIdentityChanged);
            }
            if old == new {
                return Err(FreshnessDeliveryError::UnchangedDeltaEntry);
            }
        }
        Ok(())
    }
}

/// Complete metadata for an authorized delta between two immutable source
/// generations for one mount. Entries are canonical bytewise by stable
/// projection identity. An empty entry list is a valid no-content generation
/// advancement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationDelta {
    pub format_version: u16,
    pub minimum_reader_version: u16,
    pub delta_id: String,
    pub mount_id: PortableMountId,
    pub source_connection_id: SourceConnectionId,
    pub base_generation_id: SourceGenerationId,
    pub target_generation_id: SourceGenerationId,
    pub target_complete: bool,
    pub target_inventory_sha256: String,
    pub workspace_layout_version: u16,
    pub workspace_layout_digest: LayoutDigest,
    pub entries: Vec<GenerationDeltaEntry>,
}

impl GenerationDelta {
    pub fn validate(&self) -> Result<(), FreshnessDeliveryError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_identifier("delta_id", &self.delta_id)?;
        validate_identifier("mount_id", self.mount_id.as_str())?;
        validate_identifier("source_connection_id", self.source_connection_id.as_str())?;
        validate_identifier("base_generation_id", self.base_generation_id.as_str())?;
        validate_identifier("target_generation_id", self.target_generation_id.as_str())?;
        if self.base_generation_id == self.target_generation_id {
            return Err(FreshnessDeliveryError::SameGeneration);
        }
        if !self.target_complete {
            return Err(FreshnessDeliveryError::IncompleteTargetGeneration);
        }
        validate_sha256(&self.target_inventory_sha256)?;
        if self.workspace_layout_version == 0 {
            return Err(FreshnessDeliveryError::InvalidLayoutVersion);
        }
        if self.entries.len() > MAX_GENERATION_DELTA_ENTRIES {
            return Err(FreshnessDeliveryError::TooManyDeltaEntries {
                actual: self.entries.len(),
            });
        }

        let mut previous_projection: Option<&str> = None;
        let mut claimed_paths = BTreeSet::new();
        for entry in &self.entries {
            entry.validate()?;
            let projection_id = entry
                .projection_id()
                .expect("validated entry has an identity")
                .as_str();
            if previous_projection.is_some_and(|previous| previous >= projection_id) {
                return Err(FreshnessDeliveryError::NonCanonicalDeltaOrder);
            }
            previous_projection = Some(projection_id);

            let mut entry_paths = BTreeSet::new();
            if let Some(old) = &entry.old {
                entry_paths.insert(old.logical_path.portable_collision_key());
            }
            if let Some(new) = &entry.new {
                entry_paths.insert(new.logical_path.portable_collision_key());
            }
            for path in entry_paths {
                if !claimed_paths.insert(path) {
                    return Err(FreshnessDeliveryError::CrossEntryPathReuse);
                }
            }
        }
        let changed_content_bytes = self.changed_content_bytes()?;
        if changed_content_bytes > MAX_GENERATION_DELTA_CONTENT_BYTES {
            return Err(FreshnessDeliveryError::DeltaContentTooLarge {
                actual: changed_content_bytes,
            });
        }
        Ok(())
    }

    pub fn canonical_preimage(&self) -> Result<Vec<u8>, FreshnessDeliveryError> {
        self.validate()?;
        let mut output = b"locality.generation-delta.v1\0".to_vec();
        append_u64(&mut output, u64::from(self.format_version));
        append_u64(&mut output, u64::from(self.minimum_reader_version));
        append_text(&mut output, &self.delta_id)?;
        append_text(&mut output, self.mount_id.as_str())?;
        append_text(&mut output, self.source_connection_id.as_str())?;
        append_text(&mut output, self.base_generation_id.as_str())?;
        append_text(&mut output, self.target_generation_id.as_str())?;
        append_text(
            &mut output,
            if self.target_complete {
                "true"
            } else {
                "false"
            },
        )?;
        append_text(&mut output, &self.target_inventory_sha256)?;
        append_u64(&mut output, u64::from(self.workspace_layout_version));
        append_text(&mut output, self.workspace_layout_digest.as_str())?;
        append_u64(
            &mut output,
            u64::try_from(self.entries.len())
                .map_err(|_| FreshnessDeliveryError::CanonicalValueTooLarge)?,
        );
        for entry in &self.entries {
            append_identity(&mut output, entry.old.as_ref())?;
            append_identity(&mut output, entry.new.as_ref())?;
        }
        Ok(output)
    }

    pub fn canonical_sha256(&self) -> Result<String, FreshnessDeliveryError> {
        Ok(sha256_label(&self.canonical_preimage()?))
    }

    pub fn changed_content_bytes(&self) -> Result<u64, FreshnessDeliveryError> {
        self.entries.iter().try_fold(0_u64, |total, entry| {
            total
                .checked_add(
                    entry
                        .new
                        .as_ref()
                        .map_or(0, |identity| identity.byte_length),
                )
                .ok_or(FreshnessDeliveryError::ContentLengthOverflow)
        })
    }
}

/// Terminal server receipt for the exact delta metadata. Authentication is a
/// transport responsibility; this receipt makes an authenticated response
/// replayable and rejects metadata substitution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationDeltaTerminalReceipt {
    pub format_version: u16,
    pub minimum_reader_version: u16,
    pub delta_id: String,
    pub mount_id: PortableMountId,
    pub source_connection_id: SourceConnectionId,
    pub base_generation_id: SourceGenerationId,
    pub target_generation_id: SourceGenerationId,
    pub target_inventory_sha256: String,
    pub workspace_layout_version: u16,
    pub workspace_layout_digest: LayoutDigest,
    pub delta_sha256: String,
    pub entry_count: u64,
    pub changed_content_bytes: u64,
    pub authorization_epoch: FreshnessEpoch,
    pub completed_at: String,
}

impl GenerationDeltaTerminalReceipt {
    pub fn validate(&self) -> Result<(), FreshnessDeliveryError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        validate_identifier("delta_id", &self.delta_id)?;
        validate_identifier("mount_id", self.mount_id.as_str())?;
        validate_identifier("source_connection_id", self.source_connection_id.as_str())?;
        validate_identifier("base_generation_id", self.base_generation_id.as_str())?;
        validate_identifier("target_generation_id", self.target_generation_id.as_str())?;
        validate_sha256(&self.target_inventory_sha256)?;
        validate_sha256(&self.delta_sha256)?;
        if self.workspace_layout_version == 0 {
            return Err(FreshnessDeliveryError::InvalidLayoutVersion);
        }
        validate_timestamp(&self.completed_at)
    }

    pub fn validate_against(&self, delta: &GenerationDelta) -> Result<(), FreshnessDeliveryError> {
        self.validate()?;
        delta.validate()?;
        let entry_count = u64::try_from(delta.entries.len())
            .map_err(|_| FreshnessDeliveryError::CanonicalValueTooLarge)?;
        if self.format_version != delta.format_version
            || self.minimum_reader_version != delta.minimum_reader_version
            || self.delta_id != delta.delta_id
            || self.mount_id != delta.mount_id
            || self.source_connection_id != delta.source_connection_id
            || self.base_generation_id != delta.base_generation_id
            || self.target_generation_id != delta.target_generation_id
            || self.target_inventory_sha256 != delta.target_inventory_sha256
            || self.workspace_layout_version != delta.workspace_layout_version
            || self.workspace_layout_digest != delta.workspace_layout_digest
            || self.delta_sha256 != delta.canonical_sha256()?
            || self.entry_count != entry_count
            || self.changed_content_bytes != delta.changed_content_bytes()?
        {
            return Err(FreshnessDeliveryError::ReceiptMismatch);
        }
        Ok(())
    }

    pub fn canonical_sha256(&self) -> Result<String, FreshnessDeliveryError> {
        self.validate()?;
        let mut output = b"locality.generation-delta-terminal-receipt.v1\0".to_vec();
        append_u64(&mut output, u64::from(self.format_version));
        append_u64(&mut output, u64::from(self.minimum_reader_version));
        append_text(&mut output, &self.delta_id)?;
        append_text(&mut output, self.mount_id.as_str())?;
        append_text(&mut output, self.source_connection_id.as_str())?;
        append_text(&mut output, self.base_generation_id.as_str())?;
        append_text(&mut output, self.target_generation_id.as_str())?;
        append_text(&mut output, &self.target_inventory_sha256)?;
        append_u64(&mut output, u64::from(self.workspace_layout_version));
        append_text(&mut output, self.workspace_layout_digest.as_str())?;
        append_text(&mut output, &self.delta_sha256)?;
        append_u64(&mut output, self.entry_count);
        append_u64(&mut output, self.changed_content_bytes);
        append_u64(
            &mut output,
            u64::try_from(self.authorization_epoch.get())
                .map_err(|_| FreshnessDeliveryError::CanonicalValueTooLarge)?,
        );
        append_text(&mut output, &self.completed_at)?;
        Ok(sha256_label(&output))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FreshnessDeliveryError {
    UpdateRequired { minimum: u16, supported: u16 },
    UnsupportedFormatVersion { actual: u16 },
    InvalidVersionEnvelope,
    IdentifierEmpty(&'static str),
    IdentifierTooLong(&'static str),
    InvalidTimestamp,
    InvalidSha256,
    UnknownReasonCode,
    UnknownRetryClass,
    InvalidRetryDelay,
    UnexpectedRetryDelay,
    UnknownHealthState,
    UnknownWorkerProgress,
    InvalidEpochOrder,
    UnselectableGenerationFacts,
    HealthSourceMismatch,
    EmptyDeltaEntry,
    ProjectionIdentityChanged,
    UnchangedDeltaEntry,
    SameGeneration,
    IncompleteTargetGeneration,
    InvalidLayoutVersion,
    TooManyDeltaEntries { actual: usize },
    FileContentTooLarge { actual: u64 },
    DeltaContentTooLarge { actual: u64 },
    NonCanonicalDeltaOrder,
    CrossEntryPathReuse,
    ContentLengthOverflow,
    CanonicalValueTooLarge,
    ReceiptMismatch,
}

impl Display for FreshnessDeliveryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UpdateRequired { minimum, supported } => write!(
                formatter,
                "freshness delivery requires reader version {minimum}, supported version is {supported}"
            ),
            Self::UnsupportedFormatVersion { actual } => {
                write!(
                    formatter,
                    "freshness delivery format version {actual} is unsupported"
                )
            }
            Self::InvalidVersionEnvelope => formatter.write_str("invalid version envelope"),
            Self::IdentifierEmpty(field) => write!(formatter, "{field} must not be empty"),
            Self::IdentifierTooLong(field) => write!(formatter, "{field} is too long"),
            Self::InvalidTimestamp => formatter.write_str("invalid bounded timestamp"),
            Self::InvalidSha256 => {
                formatter.write_str("digest must be `sha256:` plus 64 lowercase hexadecimal digits")
            }
            Self::UnknownReasonCode => formatter.write_str("unknown freshness reason code"),
            Self::UnknownRetryClass => formatter.write_str("unknown freshness retry class"),
            Self::InvalidRetryDelay => formatter.write_str("invalid bounded retry delay"),
            Self::UnexpectedRetryDelay => {
                formatter.write_str("retry delay is only valid for after_delay")
            }
            Self::UnknownHealthState => formatter.write_str("unknown health state"),
            Self::UnknownWorkerProgress => formatter.write_str("unknown worker progress"),
            Self::InvalidEpochOrder => formatter.write_str("invalid freshness epoch order"),
            Self::UnselectableGenerationFacts => {
                formatter.write_str("selectable generation lacks complete verified retained facts")
            }
            Self::HealthSourceMismatch => {
                formatter.write_str("provider and publication health name different sources")
            }
            Self::EmptyDeltaEntry => formatter.write_str("delta entry has no old or new identity"),
            Self::ProjectionIdentityChanged => {
                formatter.write_str("delta entry changed stable projection identity")
            }
            Self::UnchangedDeltaEntry => formatter.write_str("delta entry is unchanged"),
            Self::SameGeneration => formatter.write_str("delta base and target generations match"),
            Self::IncompleteTargetGeneration => {
                formatter.write_str("delta target generation is incomplete")
            }
            Self::InvalidLayoutVersion => formatter.write_str("workspace layout version is zero"),
            Self::TooManyDeltaEntries { actual } => write!(
                formatter,
                "delta contains {actual} entries, exceeding {MAX_GENERATION_DELTA_ENTRIES}"
            ),
            Self::FileContentTooLarge { actual } => write!(
                formatter,
                "generation file contains {actual} bytes, exceeding {MAX_GENERATION_FILE_BYTES}"
            ),
            Self::DeltaContentTooLarge { actual } => write!(
                formatter,
                "delta contains {actual} content bytes, exceeding {MAX_GENERATION_DELTA_CONTENT_BYTES}"
            ),
            Self::NonCanonicalDeltaOrder => {
                formatter.write_str("delta entries are not in canonical order")
            }
            Self::CrossEntryPathReuse => {
                formatter.write_str("delta reuses a logical path across entries")
            }
            Self::ContentLengthOverflow => formatter.write_str("delta content length overflow"),
            Self::CanonicalValueTooLarge => {
                formatter.write_str("value is too large for canonical encoding")
            }
            Self::ReceiptMismatch => formatter.write_str("terminal receipt does not match delta"),
        }
    }
}

impl std::error::Error for FreshnessDeliveryError {}

fn validate_versions(
    format_version: u16,
    minimum_reader_version: u16,
) -> Result<(), FreshnessDeliveryError> {
    if format_version == 0 || minimum_reader_version == 0 || minimum_reader_version > format_version
    {
        return Err(FreshnessDeliveryError::InvalidVersionEnvelope);
    }
    if minimum_reader_version > FRESHNESS_DELIVERY_READER_VERSION {
        return Err(FreshnessDeliveryError::UpdateRequired {
            minimum: minimum_reader_version,
            supported: FRESHNESS_DELIVERY_READER_VERSION,
        });
    }
    if format_version != GENERATION_DELTA_FORMAT_VERSION {
        return Err(FreshnessDeliveryError::UnsupportedFormatVersion {
            actual: format_version,
        });
    }
    Ok(())
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), FreshnessDeliveryError> {
    if value.is_empty() {
        return Err(FreshnessDeliveryError::IdentifierEmpty(field));
    }
    if value.len() > MAX_DELIVERY_ID_BYTES {
        return Err(FreshnessDeliveryError::IdentifierTooLong(field));
    }
    Ok(())
}

fn validate_reason(reason: Option<FreshnessReasonCode>) -> Result<(), FreshnessDeliveryError> {
    if reason == Some(FreshnessReasonCode::Unknown) {
        Err(FreshnessDeliveryError::UnknownReasonCode)
    } else {
        Ok(())
    }
}

fn validate_optional_timestamp(value: &Option<String>) -> Result<(), FreshnessDeliveryError> {
    if let Some(value) = value {
        validate_timestamp(value)
    } else {
        Ok(())
    }
}

fn validate_timestamp(value: &str) -> Result<(), FreshnessDeliveryError> {
    if value.is_empty()
        || value.len() > MAX_DELIVERY_TIMESTAMP_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        Err(FreshnessDeliveryError::InvalidTimestamp)
    } else {
        Ok(())
    }
}

fn validate_sha256(value: &str) -> Result<(), FreshnessDeliveryError> {
    if value.len() == 71
        && value.starts_with("sha256:")
        && value.as_bytes()[7..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        Ok(())
    } else {
        Err(FreshnessDeliveryError::InvalidSha256)
    }
}

fn append_identity(
    output: &mut Vec<u8>,
    identity: Option<&GenerationFileIdentity>,
) -> Result<(), FreshnessDeliveryError> {
    match identity {
        None => append_text(output, "absent"),
        Some(identity) => {
            append_text(output, "present")?;
            append_text(output, identity.projection_id.as_str())?;
            append_text(output, identity.logical_path.as_str())?;
            append_text(output, identity.content_version_id.as_str())?;
            append_text(output, &identity.content_sha256)?;
            append_u64(output, identity.byte_length);
            Ok(())
        }
    }
}

fn append_text(output: &mut Vec<u8>, value: &str) -> Result<(), FreshnessDeliveryError> {
    append_u64(
        output,
        u64::try_from(value.len()).map_err(|_| FreshnessDeliveryError::CanonicalValueTooLarge)?,
    );
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn append_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn sha256_label(value: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(value))
}
