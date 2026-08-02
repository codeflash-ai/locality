//! Strict generation-baseline sidecar for a completed generation-2 export.
//!
//! The sidecar seeds local freshness state without changing export-v2 archive
//! records. It contains only portable immutable identity and digest facts. HTTP
//! authentication, authorization, persistence, and host publication remain
//! outside this crate.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};

use locality_core::portable::{
    ContentVersionId, ExportAttemptId, LogicalPath, ProjectionId, SessionId, SourceConnectionId,
    SourceGenerationId,
};
use locality_core::workspace_layout::PortableMountId;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::OrderedSourceGeneration;
use crate::freshness_delivery::{
    FreshnessDeliveryError, GenerationFileIdentity, MAX_DELIVERY_ID_BYTES,
    MAX_GENERATION_DELTA_CONTENT_BYTES, MAX_GENERATION_DELTA_ENTRIES,
    canonical_target_inventory_sha256,
};
use crate::workspace_api_v2::WorkspaceExportOfferV2;
use crate::workspace_layout::{
    LayoutDigest, MAX_PROFILE_MOUNTS, WORKSPACE_LAYOUT_VERSION, WorkspaceProfileId,
};

pub const GENERATION_BASELINE_FORMAT_VERSION: u16 = 1;
pub const GENERATION_BASELINE_READER_VERSION: u16 = 1;
pub const GENERATION_BASELINE_V1_DOMAIN: &[u8] = b"locality.generation-baseline.v1\0";
pub const MAX_GENERATION_BASELINE_ENCODED_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_GENERATION_BASELINE_MOUNTS: usize = MAX_PROFILE_MOUNTS;
pub const MAX_GENERATION_BASELINE_FILES: usize = MAX_GENERATION_DELTA_ENTRIES;
pub const MAX_GENERATION_BASELINE_CONTENT_BYTES: u64 = MAX_GENERATION_DELTA_CONTENT_BYTES;

pub const GENERATION_BASELINE_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-baseline-v1.json");
pub const GENERATION_BASELINE_PREIMAGE_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-baseline-preimage-v1.json");

/// Complete generation state for one portable mount.
///
/// V1 deliberately carries exactly one source and one observed generation per
/// mount. Every file inherits that source/generation binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GenerationBaselineMountV1 {
    mount_id: PortableMountId,
    source_connection_id: SourceConnectionId,
    observed_generation_id: SourceGenerationId,
    target_inventory_sha256: String,
    files: Vec<GenerationFileIdentity>,
}

impl GenerationBaselineMountV1 {
    pub fn new(
        mount_id: PortableMountId,
        source_connection_id: SourceConnectionId,
        observed_generation_id: SourceGenerationId,
        files: Vec<GenerationFileIdentity>,
    ) -> Result<Self, GenerationBaselineError> {
        let target_inventory_sha256 = canonical_target_inventory_sha256(&files)?;
        Ok(Self {
            mount_id,
            source_connection_id,
            observed_generation_id,
            target_inventory_sha256,
            files,
        })
    }

    pub fn mount_id(&self) -> &PortableMountId {
        &self.mount_id
    }

    pub fn source_connection_id(&self) -> &SourceConnectionId {
        &self.source_connection_id
    }

    pub fn observed_generation_id(&self) -> &SourceGenerationId {
        &self.observed_generation_id
    }

    pub fn target_inventory_sha256(&self) -> &str {
        &self.target_inventory_sha256
    }

    pub fn files(&self) -> &[GenerationFileIdentity] {
        &self.files
    }
}

/// Immutable sidecar response for the exact full-export attempt that produced
/// the local tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GenerationBaselineResponseV1 {
    format_version: u16,
    minimum_reader_version: u16,
    profile_id: WorkspaceProfileId,
    profile_revision: u64,
    session_id: SessionId,
    export_attempt_id: ExportAttemptId,
    layout_version: u16,
    layout_digest: LayoutDigest,
    inventory_sha256: String,
    source_generations: Vec<OrderedSourceGeneration>,
    mounts: Vec<GenerationBaselineMountV1>,
    baseline_sha256: String,
}

impl GenerationBaselineResponseV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        profile_id: WorkspaceProfileId,
        profile_revision: u64,
        session_id: SessionId,
        export_attempt_id: ExportAttemptId,
        layout_version: u16,
        layout_digest: LayoutDigest,
        inventory_sha256: String,
        source_generations: Vec<OrderedSourceGeneration>,
        mounts: Vec<GenerationBaselineMountV1>,
    ) -> Result<Self, GenerationBaselineError> {
        let mut response = Self {
            format_version: GENERATION_BASELINE_FORMAT_VERSION,
            minimum_reader_version: GENERATION_BASELINE_READER_VERSION,
            profile_id,
            profile_revision,
            session_id,
            export_attempt_id,
            layout_version,
            layout_digest,
            inventory_sha256,
            source_generations,
            mounts,
            baseline_sha256: String::new(),
        };
        response.validate_shape()?;
        response.baseline_sha256 = response.recompute_baseline_sha256()?;
        response.validate()?;
        Ok(response)
    }

    /// Build the binding fields directly from the exact export-v2 offer.
    pub fn from_export_offer(
        offer: &WorkspaceExportOfferV2,
        mounts: Vec<GenerationBaselineMountV1>,
    ) -> Result<Self, GenerationBaselineError> {
        offer
            .validate()
            .map_err(|_| GenerationBaselineError::InvalidExportOffer)?;
        let sealed = offer.offer();
        let response = Self::new(
            offer.profile_id().clone(),
            offer.profile_revision(),
            sealed.session_id.clone(),
            sealed.export_attempt_id.clone(),
            offer.layout_version(),
            offer.layout_digest().clone(),
            sealed.inventory_sha256.clone(),
            sealed.source_generations.clone(),
            mounts,
        )?;
        response.validate_against_export_offer(offer)?;
        Ok(response)
    }

    /// Strict bounded decode. This validates the self-contained canonical seal
    /// but cannot establish which authenticated attempt the caller requested.
    pub fn decode_json(input: &[u8]) -> Result<Self, GenerationBaselineError> {
        if input.len() > MAX_GENERATION_BASELINE_ENCODED_BYTES {
            return Err(GenerationBaselineError::EncodingTooLarge {
                actual: input.len(),
            });
        }
        serde_json::from_slice(input)
            .map_err(|error| GenerationBaselineError::InvalidJson(error.to_string()))
    }

    /// Strict bounded decode plus the required exact-attempt comparison.
    pub fn decode_json_against_export_offer(
        input: &[u8],
        offer: &WorkspaceExportOfferV2,
    ) -> Result<Self, GenerationBaselineError> {
        let response = Self::decode_json(input)?;
        response.validate_against_export_offer(offer)?;
        Ok(response)
    }

    pub fn validate(&self) -> Result<(), GenerationBaselineError> {
        self.validate_shape()?;
        validate_sha256("baseline_sha256", &self.baseline_sha256)?;
        if self.recompute_baseline_sha256()? != self.baseline_sha256 {
            return Err(GenerationBaselineError::BaselineDigestMismatch);
        }
        let encoded_bytes = self.serialized_len()?;
        if encoded_bytes > MAX_GENERATION_BASELINE_ENCODED_BYTES {
            return Err(GenerationBaselineError::EncodingTooLarge {
                actual: encoded_bytes,
            });
        }
        Ok(())
    }

    pub fn validate_against_export_offer(
        &self,
        offer: &WorkspaceExportOfferV2,
    ) -> Result<(), GenerationBaselineError> {
        self.validate()?;
        offer
            .validate()
            .map_err(|_| GenerationBaselineError::InvalidExportOffer)?;
        let sealed = offer.offer();
        if self.profile_id != *offer.profile_id()
            || self.profile_revision != offer.profile_revision()
            || self.session_id != sealed.session_id
            || self.export_attempt_id != sealed.export_attempt_id
            || self.layout_version != offer.layout_version()
            || self.layout_digest != *offer.layout_digest()
            || self.inventory_sha256 != sealed.inventory_sha256
            || self.source_generations != sealed.source_generations
        {
            return Err(GenerationBaselineError::ExportBindingMismatch);
        }

        let (file_count, content_bytes) = self.file_totals()?;
        if file_count != sealed.file_count || content_bytes != sealed.selected_content_bytes {
            return Err(GenerationBaselineError::ExportTotalsMismatch);
        }
        Ok(())
    }

    pub fn canonical_preimage(&self) -> Result<Vec<u8>, GenerationBaselineError> {
        self.validate_shape()?;
        let mut output = GENERATION_BASELINE_V1_DOMAIN.to_vec();
        append_u64(&mut output, u64::from(self.format_version));
        append_u64(&mut output, u64::from(self.minimum_reader_version));
        append_text(&mut output, self.profile_id.as_str())?;
        append_u64(&mut output, self.profile_revision);
        append_text(&mut output, self.session_id.as_str())?;
        append_text(&mut output, self.export_attempt_id.as_str())?;
        append_u64(&mut output, u64::from(self.layout_version));
        append_text(&mut output, self.layout_digest.as_str())?;
        append_text(&mut output, &self.inventory_sha256)?;
        append_count(&mut output, self.source_generations.len())?;
        for generation in &self.source_generations {
            append_u64(&mut output, u64::from(generation.ordinal));
            append_text(&mut output, generation.source_connection_id.as_str())?;
            append_text(&mut output, generation.source_generation_id.as_str())?;
        }
        append_count(&mut output, self.mounts.len())?;
        for mount in &self.mounts {
            append_text(&mut output, mount.mount_id.as_str())?;
            append_text(&mut output, mount.source_connection_id.as_str())?;
            append_text(&mut output, mount.observed_generation_id.as_str())?;
            append_text(&mut output, &mount.target_inventory_sha256)?;
            append_count(&mut output, mount.files.len())?;
            for file in &mount.files {
                append_text(&mut output, file.projection_id.as_str())?;
                append_text(&mut output, file.logical_path.as_str())?;
                append_text(&mut output, file.content_version_id.as_str())?;
                append_text(&mut output, &file.content_sha256)?;
                append_u64(&mut output, file.byte_length);
            }
        }
        if output.len() > MAX_GENERATION_BASELINE_ENCODED_BYTES {
            return Err(GenerationBaselineError::CanonicalPreimageTooLarge {
                actual: output.len(),
            });
        }
        Ok(output)
    }

    pub fn recompute_baseline_sha256(&self) -> Result<String, GenerationBaselineError> {
        Ok(sha256_label(&self.canonical_preimage()?))
    }

    pub fn serialized_len(&self) -> Result<usize, GenerationBaselineError> {
        let mut writer = JsonLengthWriter::default();
        serde_json::to_writer(&mut writer, self)
            .map_err(|_| GenerationBaselineError::CanonicalValueTooLarge)?;
        Ok(writer.len)
    }

    pub fn format_version(&self) -> u16 {
        self.format_version
    }

    pub fn minimum_reader_version(&self) -> u16 {
        self.minimum_reader_version
    }

    pub fn profile_id(&self) -> &WorkspaceProfileId {
        &self.profile_id
    }

    pub fn profile_revision(&self) -> u64 {
        self.profile_revision
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn export_attempt_id(&self) -> &ExportAttemptId {
        &self.export_attempt_id
    }

    pub fn layout_version(&self) -> u16 {
        self.layout_version
    }

    pub fn layout_digest(&self) -> &LayoutDigest {
        &self.layout_digest
    }

    pub fn inventory_sha256(&self) -> &str {
        &self.inventory_sha256
    }

    pub fn source_generations(&self) -> &[OrderedSourceGeneration] {
        &self.source_generations
    }

    pub fn mounts(&self) -> &[GenerationBaselineMountV1] {
        &self.mounts
    }

    pub fn baseline_sha256(&self) -> &str {
        &self.baseline_sha256
    }

    fn validate_shape(&self) -> Result<(), GenerationBaselineError> {
        validate_versions(self.format_version, self.minimum_reader_version)?;
        if self.profile_revision == 0 {
            return Err(GenerationBaselineError::ZeroProfileRevision);
        }
        if self.layout_version != WORKSPACE_LAYOUT_VERSION {
            return Err(GenerationBaselineError::UnsupportedLayoutVersion {
                actual: self.layout_version,
            });
        }
        validate_identifier("session_id", self.session_id.as_str())?;
        validate_identifier("export_attempt_id", self.export_attempt_id.as_str())?;
        validate_sha256("inventory_sha256", &self.inventory_sha256)?;

        if self.source_generations.is_empty()
            || self.source_generations.len() > MAX_GENERATION_BASELINE_MOUNTS
        {
            return Err(GenerationBaselineError::SourceGenerationCount {
                actual: self.source_generations.len(),
            });
        }
        let mut generation_by_source = BTreeMap::new();
        let mut generation_ids = BTreeSet::new();
        for (index, generation) in self.source_generations.iter().enumerate() {
            let expected = u32::try_from(index)
                .map_err(|_| GenerationBaselineError::CanonicalValueTooLarge)?;
            if generation.ordinal != expected {
                return Err(GenerationBaselineError::NonCanonicalSourceGenerationOrder {
                    index,
                    actual: generation.ordinal,
                });
            }
            validate_identifier(
                "source_connection_id",
                generation.source_connection_id.as_str(),
            )?;
            validate_identifier(
                "source_generation_id",
                generation.source_generation_id.as_str(),
            )?;
            if generation_by_source
                .insert(
                    &generation.source_connection_id,
                    &generation.source_generation_id,
                )
                .is_some()
            {
                return Err(GenerationBaselineError::DuplicateSourceConnection);
            }
            if !generation_ids.insert(&generation.source_generation_id) {
                return Err(GenerationBaselineError::DuplicateSourceGeneration);
            }
        }

        if self.mounts.is_empty() || self.mounts.len() > MAX_GENERATION_BASELINE_MOUNTS {
            return Err(GenerationBaselineError::MountCount {
                actual: self.mounts.len(),
            });
        }
        let mut previous_mount_id: Option<&PortableMountId> = None;
        let mut mounted_sources = BTreeSet::new();
        let mut projection_ids = BTreeSet::new();
        let mut file_count = 0_usize;
        let mut content_bytes = 0_u64;
        for (index, mount) in self.mounts.iter().enumerate() {
            if previous_mount_id.is_some_and(|previous| previous >= &mount.mount_id) {
                return Err(GenerationBaselineError::NonCanonicalMountOrder { index });
            }
            previous_mount_id = Some(&mount.mount_id);
            validate_identifier("mount_id", mount.mount_id.as_str())?;
            validate_identifier("source_connection_id", mount.source_connection_id.as_str())?;
            validate_identifier(
                "observed_generation_id",
                mount.observed_generation_id.as_str(),
            )?;
            validate_sha256("target_inventory_sha256", &mount.target_inventory_sha256)?;

            let Some(expected_generation) = generation_by_source.get(&mount.source_connection_id)
            else {
                return Err(GenerationBaselineError::MountSourceNotInGenerationVector {
                    mount_id: mount.mount_id.as_str().to_string(),
                });
            };
            if **expected_generation != mount.observed_generation_id {
                return Err(GenerationBaselineError::MountGenerationMismatch {
                    mount_id: mount.mount_id.as_str().to_string(),
                });
            }
            mounted_sources.insert(&mount.source_connection_id);

            let target_inventory_sha256 = canonical_target_inventory_sha256(&mount.files)?;
            if target_inventory_sha256 != mount.target_inventory_sha256 {
                return Err(GenerationBaselineError::TargetInventoryMismatch {
                    mount_id: mount.mount_id.as_str().to_string(),
                });
            }
            file_count = file_count
                .checked_add(mount.files.len())
                .ok_or(GenerationBaselineError::CanonicalValueTooLarge)?;
            if file_count > MAX_GENERATION_BASELINE_FILES {
                return Err(GenerationBaselineError::FileCount { actual: file_count });
            }
            for file in &mount.files {
                validate_identifier("projection_id", file.projection_id.as_str())?;
                validate_identifier("content_version_id", file.content_version_id.as_str())?;
                if !projection_ids.insert(&file.projection_id) {
                    return Err(GenerationBaselineError::DuplicateProjectionId {
                        projection_id: file.projection_id.as_str().to_string(),
                    });
                }
                content_bytes = content_bytes
                    .checked_add(file.byte_length)
                    .ok_or(GenerationBaselineError::ContentLengthOverflow)?;
                if content_bytes > MAX_GENERATION_BASELINE_CONTENT_BYTES {
                    return Err(GenerationBaselineError::ContentBytesTooLarge {
                        actual: content_bytes,
                    });
                }
            }
        }
        if mounted_sources != generation_by_source.keys().copied().collect() {
            return Err(GenerationBaselineError::SourceSetMismatch);
        }
        Ok(())
    }

    fn file_totals(&self) -> Result<(u64, u64), GenerationBaselineError> {
        self.mounts
            .iter()
            .try_fold((0_u64, 0_u64), |(files, bytes), mount| {
                let mount_files = u64::try_from(mount.files.len())
                    .map_err(|_| GenerationBaselineError::CanonicalValueTooLarge)?;
                let files = files
                    .checked_add(mount_files)
                    .ok_or(GenerationBaselineError::CanonicalValueTooLarge)?;
                let bytes = mount.files.iter().try_fold(bytes, |total, file| {
                    total
                        .checked_add(file.byte_length)
                        .ok_or(GenerationBaselineError::ContentLengthOverflow)
                })?;
                Ok((files, bytes))
            })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationBaselineResponseV1Wire {
    format_version: u16,
    minimum_reader_version: u16,
    profile_id: WorkspaceProfileId,
    profile_revision: u64,
    session_id: SessionId,
    export_attempt_id: ExportAttemptId,
    layout_version: u16,
    layout_digest: LayoutDigest,
    inventory_sha256: String,
    source_generations: Vec<OrderedSourceGenerationWire>,
    mounts: Vec<GenerationBaselineMountV1Wire>,
    baseline_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderedSourceGenerationWire {
    ordinal: u32,
    source_connection_id: SourceConnectionId,
    source_generation_id: SourceGenerationId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationBaselineMountV1Wire {
    mount_id: PortableMountId,
    source_connection_id: SourceConnectionId,
    observed_generation_id: SourceGenerationId,
    target_inventory_sha256: String,
    files: Vec<GenerationBaselineFileV1Wire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationBaselineFileV1Wire {
    projection_id: ProjectionId,
    logical_path: LogicalPath,
    content_version_id: ContentVersionId,
    content_sha256: String,
    byte_length: u64,
}

impl<'de> Deserialize<'de> for GenerationBaselineResponseV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = GenerationBaselineResponseV1Wire::deserialize(deserializer)?;
        let response = Self {
            format_version: wire.format_version,
            minimum_reader_version: wire.minimum_reader_version,
            profile_id: wire.profile_id,
            profile_revision: wire.profile_revision,
            session_id: wire.session_id,
            export_attempt_id: wire.export_attempt_id,
            layout_version: wire.layout_version,
            layout_digest: wire.layout_digest,
            inventory_sha256: wire.inventory_sha256,
            source_generations: wire
                .source_generations
                .into_iter()
                .map(|generation| OrderedSourceGeneration {
                    ordinal: generation.ordinal,
                    source_connection_id: generation.source_connection_id,
                    source_generation_id: generation.source_generation_id,
                })
                .collect(),
            mounts: wire
                .mounts
                .into_iter()
                .map(|mount| GenerationBaselineMountV1 {
                    mount_id: mount.mount_id,
                    source_connection_id: mount.source_connection_id,
                    observed_generation_id: mount.observed_generation_id,
                    target_inventory_sha256: mount.target_inventory_sha256,
                    files: mount
                        .files
                        .into_iter()
                        .map(|file| GenerationFileIdentity {
                            projection_id: file.projection_id,
                            logical_path: file.logical_path,
                            content_version_id: file.content_version_id,
                            content_sha256: file.content_sha256,
                            byte_length: file.byte_length,
                        })
                        .collect(),
                })
                .collect(),
            baseline_sha256: wire.baseline_sha256,
        };
        response.validate().map_err(serde::de::Error::custom)?;
        Ok(response)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenerationBaselineError {
    UpdateRequired { minimum: u16, supported: u16 },
    UnsupportedFormatVersion { actual: u16 },
    InvalidVersionEnvelope,
    ZeroProfileRevision,
    UnsupportedLayoutVersion { actual: u16 },
    IdentifierEmpty(&'static str),
    IdentifierTooLong(&'static str),
    InvalidSha256(&'static str),
    SourceGenerationCount { actual: usize },
    NonCanonicalSourceGenerationOrder { index: usize, actual: u32 },
    DuplicateSourceConnection,
    DuplicateSourceGeneration,
    MountCount { actual: usize },
    NonCanonicalMountOrder { index: usize },
    MountSourceNotInGenerationVector { mount_id: String },
    MountGenerationMismatch { mount_id: String },
    SourceSetMismatch,
    TargetInventoryMismatch { mount_id: String },
    DuplicateProjectionId { projection_id: String },
    FileCount { actual: usize },
    ContentLengthOverflow,
    ContentBytesTooLarge { actual: u64 },
    EncodingTooLarge { actual: usize },
    CanonicalPreimageTooLarge { actual: usize },
    CanonicalValueTooLarge,
    BaselineDigestMismatch,
    InvalidExportOffer,
    ExportBindingMismatch,
    ExportTotalsMismatch,
    InvalidJson(String),
    TargetInventory(FreshnessDeliveryError),
}

impl Display for GenerationBaselineError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UpdateRequired { minimum, supported } => write!(
                formatter,
                "generation baseline requires reader version {minimum}, supported version is {supported}"
            ),
            Self::UnsupportedFormatVersion { actual } => {
                write!(formatter, "generation baseline format version {actual} is unsupported")
            }
            Self::InvalidVersionEnvelope => formatter.write_str("invalid version envelope"),
            Self::ZeroProfileRevision => formatter.write_str("profile revision must be positive"),
            Self::UnsupportedLayoutVersion { actual } => {
                write!(formatter, "workspace layout version {actual} is unsupported")
            }
            Self::IdentifierEmpty(field) => write!(formatter, "{field} must not be empty"),
            Self::IdentifierTooLong(field) => write!(formatter, "{field} is too long"),
            Self::InvalidSha256(field) => write!(
                formatter,
                "{field} must be `sha256:` plus 64 lowercase hexadecimal digits"
            ),
            Self::SourceGenerationCount { actual } => write!(
                formatter,
                "generation baseline has {actual} source generations; expected 1 through {MAX_GENERATION_BASELINE_MOUNTS}"
            ),
            Self::NonCanonicalSourceGenerationOrder { index, actual } => write!(
                formatter,
                "source generation at index {index} has ordinal {actual}; expected {index}"
            ),
            Self::DuplicateSourceConnection => {
                formatter.write_str("source-generation vector repeats a source connection")
            }
            Self::DuplicateSourceGeneration => {
                formatter.write_str("source-generation vector repeats a generation ID")
            }
            Self::MountCount { actual } => write!(
                formatter,
                "generation baseline has {actual} mounts; expected 1 through {MAX_GENERATION_BASELINE_MOUNTS}"
            ),
            Self::NonCanonicalMountOrder { index } => write!(
                formatter,
                "mount at index {index} is not in exact mount-ID byte order"
            ),
            Self::MountSourceNotInGenerationVector { mount_id } => write!(
                formatter,
                "mount `{mount_id}` references a source absent from the generation vector"
            ),
            Self::MountGenerationMismatch { mount_id } => write!(
                formatter,
                "mount `{mount_id}` observed a generation different from its source vector entry"
            ),
            Self::SourceSetMismatch => formatter.write_str(
                "mount sources do not exactly cover the source-generation vector",
            ),
            Self::TargetInventoryMismatch { mount_id } => write!(
                formatter,
                "mount `{mount_id}` target inventory digest does not match its files"
            ),
            Self::DuplicateProjectionId { projection_id } => write!(
                formatter,
                "projection ID `{projection_id}` occurs in more than one baseline file"
            ),
            Self::FileCount { actual } => write!(
                formatter,
                "generation baseline has {actual} files, exceeding {MAX_GENERATION_BASELINE_FILES}"
            ),
            Self::ContentLengthOverflow => formatter.write_str("content byte total overflow"),
            Self::ContentBytesTooLarge { actual } => write!(
                formatter,
                "generation baseline describes {actual} content bytes, exceeding {MAX_GENERATION_BASELINE_CONTENT_BYTES}"
            ),
            Self::EncodingTooLarge { actual } => write!(
                formatter,
                "generation baseline encoding is {actual} bytes, exceeding {MAX_GENERATION_BASELINE_ENCODED_BYTES}"
            ),
            Self::CanonicalPreimageTooLarge { actual } => write!(
                formatter,
                "generation baseline canonical preimage is {actual} bytes, exceeding {MAX_GENERATION_BASELINE_ENCODED_BYTES}"
            ),
            Self::CanonicalValueTooLarge => {
                formatter.write_str("value is too large for canonical encoding")
            }
            Self::BaselineDigestMismatch => {
                formatter.write_str("generation baseline digest does not match its canonical preimage")
            }
            Self::InvalidExportOffer => formatter.write_str("export offer is invalid"),
            Self::ExportBindingMismatch => formatter.write_str(
                "generation baseline does not match the exact profile, session, layout, inventory, attempt, and generation vector",
            ),
            Self::ExportTotalsMismatch => formatter.write_str(
                "generation baseline file and content-byte totals do not match the export offer",
            ),
            Self::InvalidJson(error) => write!(formatter, "invalid generation baseline JSON: {error}"),
            Self::TargetInventory(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for GenerationBaselineError {}

impl From<FreshnessDeliveryError> for GenerationBaselineError {
    fn from(error: FreshnessDeliveryError) -> Self {
        Self::TargetInventory(error)
    }
}

fn validate_versions(
    format_version: u16,
    minimum_reader_version: u16,
) -> Result<(), GenerationBaselineError> {
    if format_version == 0 || minimum_reader_version == 0 || minimum_reader_version > format_version
    {
        return Err(GenerationBaselineError::InvalidVersionEnvelope);
    }
    if minimum_reader_version > GENERATION_BASELINE_READER_VERSION {
        return Err(GenerationBaselineError::UpdateRequired {
            minimum: minimum_reader_version,
            supported: GENERATION_BASELINE_READER_VERSION,
        });
    }
    if format_version != GENERATION_BASELINE_FORMAT_VERSION {
        return Err(GenerationBaselineError::UnsupportedFormatVersion {
            actual: format_version,
        });
    }
    Ok(())
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), GenerationBaselineError> {
    if value.is_empty() {
        return Err(GenerationBaselineError::IdentifierEmpty(field));
    }
    if value.len() > MAX_DELIVERY_ID_BYTES {
        return Err(GenerationBaselineError::IdentifierTooLong(field));
    }
    Ok(())
}

fn validate_sha256(field: &'static str, value: &str) -> Result<(), GenerationBaselineError> {
    if value.len() == 71
        && value.starts_with("sha256:")
        && value.as_bytes()[7..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        Ok(())
    } else {
        Err(GenerationBaselineError::InvalidSha256(field))
    }
}

fn append_count(output: &mut Vec<u8>, count: usize) -> Result<(), GenerationBaselineError> {
    append_u64(
        output,
        u64::try_from(count).map_err(|_| GenerationBaselineError::CanonicalValueTooLarge)?,
    );
    Ok(())
}

fn append_text(output: &mut Vec<u8>, value: &str) -> Result<(), GenerationBaselineError> {
    append_count(output, value.len())?;
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn append_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_be_bytes());
}

fn sha256_label(value: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(value))
}

#[derive(Default)]
struct JsonLengthWriter {
    len: usize,
}

impl std::io::Write for JsonLengthWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.len = self
            .len
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("JSON length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
