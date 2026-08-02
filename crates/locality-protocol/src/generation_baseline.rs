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

use crate::MAX_EXPORT_V2_PAX_VALUE_BYTES;
use crate::OrderedSourceGeneration;
use crate::freshness_delivery::{
    GenerationFileIdentity, MAX_DELIVERY_ID_BYTES, MAX_GENERATION_FILE_BYTES,
};
use crate::workspace_api_v2::{WorkspaceExportOfferV2, WorkspaceProfileSessionV2};
use crate::workspace_export_v2::{
    WorkspaceNamespacedExportRecordV2, WorkspaceNamespacedInventoryV2,
};
use crate::workspace_layout::{
    LayoutDigest, MAX_PROFILE_MOUNTS, MAX_PROFILE_SCOPE_BINDINGS, WORKSPACE_LAYOUT_VERSION,
    WorkspaceProfileId,
};

pub const GENERATION_BASELINE_FORMAT_VERSION: u16 = 1;
pub const GENERATION_BASELINE_READER_VERSION: u16 = 1;
pub const GENERATION_BASELINE_V1_DOMAIN: &[u8] = b"locality.generation-baseline.v1\0";
pub const GENERATION_TARGET_INVENTORY_V1_DOMAIN: &[u8] =
    b"locality.generation-target-inventory.v1\0";

/// Content-version identity is sidecar-only and therefore cannot inherit an
/// export-record bound. V1 deliberately uses the frozen export-v2 PAX scalar
/// ceiling as its negotiated implementation capability.
pub const MAX_GENERATION_BASELINE_CONTENT_VERSION_ID_BYTES: usize = MAX_EXPORT_V2_PAX_VALUE_BYTES;

const GENERATION_BASELINE_JSON_BASE_BYTES: usize = 64 * 1024;
const GENERATION_BASELINE_JSON_PER_SOURCE_STATE_BYTES: usize = 512;
const GENERATION_BASELINE_JSON_PER_FILE_BYTES: usize = 512;
const MAX_JSON_ESCAPE_EXPANSION: usize = 6;

pub const GENERATION_BASELINE_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-baseline-v1.json");
pub const GENERATION_BASELINE_PREIMAGE_V1_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/generation-baseline-preimage-v1.json");

/// Whether this source state can use the existing generation-delta V1 reader.
///
/// A valid negotiated export may contain a file or identifier beyond the
/// generation-delta V1 implementation ceilings. Such a baseline remains exact
/// but is explicitly full-export-only instead of being rejected or silently
/// accepted as delta-capable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationBaselineRefreshModeV1 {
    GenerationDeltaV1,
    FullExportOnly,
}

/// Exact observed generation and target inventory for one source within one
/// mount. Shared mounts carry one of these records for every authorized source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GenerationBaselineSourceV1 {
    source_connection_id: SourceConnectionId,
    observed_generation_id: SourceGenerationId,
    target_inventory_sha256: String,
    refresh_mode: GenerationBaselineRefreshModeV1,
    files: Vec<GenerationFileIdentity>,
}

impl GenerationBaselineSourceV1 {
    pub fn new(
        source_connection_id: SourceConnectionId,
        observed_generation_id: SourceGenerationId,
        files: Vec<GenerationFileIdentity>,
    ) -> Result<Self, GenerationBaselineError> {
        validate_baseline_files(&files)?;
        let target_inventory_sha256 = baseline_target_inventory_sha256(&files)?;
        let refresh_mode =
            refresh_mode_for_source_fields(&source_connection_id, &observed_generation_id, &files);
        Ok(Self {
            source_connection_id,
            observed_generation_id,
            target_inventory_sha256,
            refresh_mode,
            files,
        })
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

    pub fn refresh_mode(&self) -> GenerationBaselineRefreshModeV1 {
        self.refresh_mode
    }

    pub fn files(&self) -> &[GenerationFileIdentity] {
        &self.files
    }
}

/// Complete source-generation state for one portable mount.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GenerationBaselineMountV1 {
    mount_id: PortableMountId,
    sources: Vec<GenerationBaselineSourceV1>,
}

impl GenerationBaselineMountV1 {
    pub fn new(
        mount_id: PortableMountId,
        mut sources: Vec<GenerationBaselineSourceV1>,
    ) -> Result<Self, GenerationBaselineError> {
        if sources.is_empty() || sources.len() > MAX_PROFILE_SCOPE_BINDINGS {
            return Err(GenerationBaselineError::MountSourceCount {
                mount_id: mount_id.as_str().to_string(),
                actual: sources.len(),
            });
        }
        for source in &mut sources {
            source.refresh_mode = refresh_mode_for_source(&mount_id, source);
        }
        Ok(Self { mount_id, sources })
    }

    pub fn mount_id(&self) -> &PortableMountId {
        &self.mount_id
    }

    pub fn sources(&self) -> &[GenerationBaselineSourceV1] {
        &self.sources
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

    /// Build and validate against the exact sealed session, offer, and
    /// recomputed canonical export inventory.
    pub fn from_export(
        session: &WorkspaceProfileSessionV2,
        offer: &WorkspaceExportOfferV2,
        inventory: &WorkspaceNamespacedInventoryV2,
        mounts: Vec<GenerationBaselineMountV1>,
    ) -> Result<Self, GenerationBaselineError> {
        validate_export_context(session, offer, inventory)?;
        let sealed = offer.offer();
        let response = Self::new(
            session.profile_id().clone(),
            session.profile_revision(),
            sealed.session_id.clone(),
            sealed.export_attempt_id.clone(),
            session.session_layout().layout_version(),
            session.session_layout().layout_digest().clone(),
            inventory.inventory_sha256().to_string(),
            sealed.source_generations.clone(),
            mounts,
        )?;
        response.validate_against_export(session, offer, inventory)?;
        Ok(response)
    }

    /// Strict bounded decode against the exact authoritative export context.
    /// There is intentionally no unbound network decoder.
    pub fn decode_json_against_export(
        input: &[u8],
        session: &WorkspaceProfileSessionV2,
        offer: &WorkspaceExportOfferV2,
        inventory: &WorkspaceNamespacedInventoryV2,
    ) -> Result<Self, GenerationBaselineError> {
        let maximum = maximum_encoded_bytes_for_export(session, offer, inventory)?;
        if input.len() > maximum {
            return Err(GenerationBaselineError::EncodingTooLarge {
                actual: input.len(),
                maximum,
            });
        }
        let response: Self = serde_json::from_slice(input)
            .map_err(|error| GenerationBaselineError::InvalidJson(error.to_string()))?;
        response.validate_against_export(session, offer, inventory)?;
        Ok(response)
    }

    pub fn validate(&self) -> Result<(), GenerationBaselineError> {
        self.validate_shape()?;
        validate_sha256("baseline_sha256", &self.baseline_sha256)?;
        if self.recompute_baseline_sha256()? != self.baseline_sha256 {
            return Err(GenerationBaselineError::BaselineDigestMismatch);
        }
        Ok(())
    }

    /// Validate every overlapping file identity against the recomputed export
    /// inventory. Content-version IDs are supplied by the authenticated
    /// endpoint and are committed by both per-source target digests and the
    /// whole-response baseline digest.
    pub fn validate_against_export(
        &self,
        session: &WorkspaceProfileSessionV2,
        offer: &WorkspaceExportOfferV2,
        inventory: &WorkspaceNamespacedInventoryV2,
    ) -> Result<(), GenerationBaselineError> {
        self.validate()?;
        let expected_mounts = validate_export_context(session, offer, inventory)?;
        let sealed = offer.offer();
        if self.profile_id != *session.profile_id()
            || self.profile_revision != session.profile_revision()
            || self.session_id != *session.session_id()
            || self.session_id != sealed.session_id
            || self.export_attempt_id != sealed.export_attempt_id
            || self.layout_version != session.session_layout().layout_version()
            || self.layout_version != offer.layout_version()
            || self.layout_digest != *session.session_layout().layout_digest()
            || self.layout_digest != *offer.layout_digest()
            || self.inventory_sha256 != inventory.inventory_sha256()
            || self.inventory_sha256 != sealed.inventory_sha256
            || self.source_generations != sealed.source_generations
        {
            return Err(GenerationBaselineError::ExportBindingMismatch);
        }

        if self.mounts.len() != expected_mounts.len() {
            return Err(GenerationBaselineError::MountSetMismatch);
        }
        for (mount, expected) in self.mounts.iter().zip(&expected_mounts) {
            if mount.mount_id != expected.mount_id
                || mount
                    .sources
                    .iter()
                    .map(|source| &source.source_connection_id)
                    .ne(expected.source_connection_ids.iter())
            {
                return Err(GenerationBaselineError::MountSourceSetMismatch {
                    mount_id: mount.mount_id.as_str().to_string(),
                });
            }
        }

        let expected_files = inventory
            .records()
            .iter()
            .filter_map(|record| match record {
                WorkspaceNamespacedExportRecordV2::File {
                    mount_id,
                    logical_path,
                    projection_id,
                    source_connection_id,
                    content_sha256,
                    byte_length,
                    ..
                } => Some((
                    projection_id,
                    (
                        mount_id,
                        source_connection_id,
                        logical_path,
                        content_sha256,
                        *byte_length,
                    ),
                )),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        let mut matched_projection_ids = BTreeSet::new();
        for mount in &self.mounts {
            for source in &mount.sources {
                for file in &source.files {
                    let Some((
                        expected_mount,
                        expected_source,
                        expected_path,
                        expected_sha256,
                        expected_byte_length,
                    )) = expected_files.get(&file.projection_id)
                    else {
                        return Err(GenerationBaselineError::InventoryFileMismatch {
                            projection_id: file.projection_id.as_str().to_string(),
                        });
                    };
                    if *expected_mount != &mount.mount_id
                        || *expected_source != &source.source_connection_id
                        || *expected_path != &file.logical_path
                        || *expected_sha256 != &file.content_sha256
                        || *expected_byte_length != file.byte_length
                    {
                        return Err(GenerationBaselineError::InventoryFileMismatch {
                            projection_id: file.projection_id.as_str().to_string(),
                        });
                    }
                    matched_projection_ids.insert(&file.projection_id);
                }
            }
        }
        if matched_projection_ids.len() != expected_files.len() {
            return Err(GenerationBaselineError::InventoryFilesMissing);
        }

        let maximum = maximum_encoded_bytes_for_export(session, offer, inventory)?;
        let actual = self.serialized_len()?;
        if actual > maximum {
            return Err(GenerationBaselineError::EncodingTooLarge { actual, maximum });
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
            append_count(&mut output, mount.sources.len())?;
            for source in &mount.sources {
                append_text(&mut output, source.source_connection_id.as_str())?;
                append_text(&mut output, source.observed_generation_id.as_str())?;
                append_text(&mut output, &source.target_inventory_sha256)?;
                append_text(&mut output, refresh_mode_label(source.refresh_mode))?;
                append_count(&mut output, source.files.len())?;
                for file in &source.files {
                    append_text(&mut output, file.projection_id.as_str())?;
                    append_text(&mut output, file.logical_path.as_str())?;
                    append_text(&mut output, file.content_version_id.as_str())?;
                    append_text(&mut output, &file.content_sha256)?;
                    append_u64(&mut output, file.byte_length);
                }
            }
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
        validate_nonempty("session_id", self.session_id.as_str())?;
        validate_nonempty("export_attempt_id", self.export_attempt_id.as_str())?;
        validate_sha256("inventory_sha256", &self.inventory_sha256)?;

        if self.source_generations.is_empty()
            || self.source_generations.len() > MAX_PROFILE_SCOPE_BINDINGS
        {
            return Err(GenerationBaselineError::SourceGenerationCount {
                actual: self.source_generations.len(),
            });
        }
        let mut generation_by_source = BTreeMap::new();
        let mut ordinal_by_source = BTreeMap::new();
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
            validate_nonempty(
                "source_connection_id",
                generation.source_connection_id.as_str(),
            )?;
            validate_nonempty(
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
            ordinal_by_source.insert(&generation.source_connection_id, generation.ordinal);
            if !generation_ids.insert(&generation.source_generation_id) {
                return Err(GenerationBaselineError::DuplicateSourceGeneration);
            }
        }

        if self.mounts.is_empty() || self.mounts.len() > MAX_PROFILE_MOUNTS {
            return Err(GenerationBaselineError::MountCount {
                actual: self.mounts.len(),
            });
        }
        let mut previous_mount_id: Option<&PortableMountId> = None;
        let mut mounted_sources = BTreeSet::new();
        let mut projection_ids = BTreeSet::new();
        let mut source_state_count = 0_usize;
        let mut content_bytes = 0_u64;
        for (mount_index, mount) in self.mounts.iter().enumerate() {
            if previous_mount_id.is_some_and(|previous| previous >= &mount.mount_id) {
                return Err(GenerationBaselineError::NonCanonicalMountOrder { mount_index });
            }
            previous_mount_id = Some(&mount.mount_id);
            validate_nonempty("mount_id", mount.mount_id.as_str())?;
            if mount.sources.is_empty() {
                return Err(GenerationBaselineError::MountSourceCount {
                    mount_id: mount.mount_id.as_str().to_string(),
                    actual: 0,
                });
            }
            source_state_count = source_state_count
                .checked_add(mount.sources.len())
                .ok_or(GenerationBaselineError::CanonicalValueTooLarge)?;
            if source_state_count > MAX_PROFILE_SCOPE_BINDINGS {
                return Err(GenerationBaselineError::SourceStateCount {
                    actual: source_state_count,
                });
            }

            let mut previous_source_ordinal = None;
            for (source_index, source) in mount.sources.iter().enumerate() {
                validate_nonempty("source_connection_id", source.source_connection_id.as_str())?;
                validate_nonempty(
                    "observed_generation_id",
                    source.observed_generation_id.as_str(),
                )?;
                validate_sha256("target_inventory_sha256", &source.target_inventory_sha256)?;
                let Some(expected_generation) =
                    generation_by_source.get(&source.source_connection_id)
                else {
                    return Err(GenerationBaselineError::MountSourceNotInGenerationVector {
                        mount_id: mount.mount_id.as_str().to_string(),
                    });
                };
                if **expected_generation != source.observed_generation_id {
                    return Err(GenerationBaselineError::MountGenerationMismatch {
                        mount_id: mount.mount_id.as_str().to_string(),
                        source_connection_id: source.source_connection_id.as_str().to_string(),
                    });
                }
                let source_ordinal = ordinal_by_source[&source.source_connection_id];
                if previous_source_ordinal.is_some_and(|previous| previous >= source_ordinal) {
                    return Err(GenerationBaselineError::NonCanonicalMountSourceOrder {
                        mount_index,
                        source_index,
                    });
                }
                previous_source_ordinal = Some(source_ordinal);
                mounted_sources.insert(&source.source_connection_id);

                validate_baseline_files(&source.files)?;
                if baseline_target_inventory_sha256(&source.files)?
                    != source.target_inventory_sha256
                {
                    return Err(GenerationBaselineError::TargetInventoryMismatch {
                        mount_id: mount.mount_id.as_str().to_string(),
                        source_connection_id: source.source_connection_id.as_str().to_string(),
                    });
                }
                if refresh_mode_for_source(&mount.mount_id, source) != source.refresh_mode {
                    return Err(GenerationBaselineError::RefreshModeMismatch {
                        mount_id: mount.mount_id.as_str().to_string(),
                        source_connection_id: source.source_connection_id.as_str().to_string(),
                    });
                }
                for file in &source.files {
                    if !projection_ids.insert(&file.projection_id) {
                        return Err(GenerationBaselineError::DuplicateProjectionId {
                            projection_id: file.projection_id.as_str().to_string(),
                        });
                    }
                    content_bytes = content_bytes
                        .checked_add(file.byte_length)
                        .ok_or(GenerationBaselineError::ContentLengthOverflow)?;
                }
            }
        }
        if mounted_sources != generation_by_source.keys().copied().collect() {
            return Err(GenerationBaselineError::SourceSetMismatch);
        }
        Ok(())
    }
}

/// Derive a raw JSON ceiling from the exact verified inventory and negotiated
/// attempt, rather than imposing a lower static file/content limit.
pub fn maximum_encoded_bytes_for_export(
    session: &WorkspaceProfileSessionV2,
    offer: &WorkspaceExportOfferV2,
    inventory: &WorkspaceNamespacedInventoryV2,
) -> Result<usize, GenerationBaselineError> {
    let expected_mounts = validate_export_context(session, offer, inventory)?;
    let mut maximum = GENERATION_BASELINE_JSON_BASE_BYTES;
    for value in [
        session.profile_id().as_str(),
        session.session_id().as_str(),
        offer.offer().export_attempt_id.as_str(),
        session.session_layout().layout_digest().as_str(),
        inventory.inventory_sha256(),
    ] {
        add_escaped_bytes(&mut maximum, value.len())?;
    }
    for generation in &offer.offer().source_generations {
        add_escaped_bytes(&mut maximum, generation.source_connection_id.as_str().len())?;
        add_escaped_bytes(&mut maximum, generation.source_generation_id.as_str().len())?;
    }
    for mount in &expected_mounts {
        add_escaped_bytes(&mut maximum, mount.mount_id.as_str().len())?;
        for source in &mount.source_connection_ids {
            maximum = maximum
                .checked_add(GENERATION_BASELINE_JSON_PER_SOURCE_STATE_BYTES)
                .ok_or(GenerationBaselineError::EncodedLimitOverflow)?;
            add_escaped_bytes(&mut maximum, source.as_str().len())?;
        }
    }
    for record in inventory.records() {
        if let WorkspaceNamespacedExportRecordV2::File {
            projection_id,
            logical_path,
            content_sha256,
            ..
        } = record
        {
            maximum = maximum
                .checked_add(GENERATION_BASELINE_JSON_PER_FILE_BYTES)
                .ok_or(GenerationBaselineError::EncodedLimitOverflow)?;
            for length in [
                projection_id.as_str().len(),
                logical_path.as_str().len(),
                MAX_GENERATION_BASELINE_CONTENT_VERSION_ID_BYTES,
                content_sha256.len(),
            ] {
                add_escaped_bytes(&mut maximum, length)?;
            }
        }
    }
    Ok(maximum)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExpectedMountSources {
    mount_id: PortableMountId,
    source_connection_ids: Vec<SourceConnectionId>,
}

fn validate_export_context(
    session: &WorkspaceProfileSessionV2,
    offer: &WorkspaceExportOfferV2,
    inventory: &WorkspaceNamespacedInventoryV2,
) -> Result<Vec<ExpectedMountSources>, GenerationBaselineError> {
    session
        .validate()
        .map_err(|_| GenerationBaselineError::InvalidExportContext)?;
    offer
        .validate()
        .map_err(|_| GenerationBaselineError::InvalidExportContext)?;
    if session.profile_id() != offer.profile_id()
        || session.profile_revision() != offer.profile_revision()
        || session.session_id() != &offer.offer().session_id
        || session.session_layout().layout_version() != offer.layout_version()
        || session.session_layout().layout_digest() != offer.layout_digest()
    {
        return Err(GenerationBaselineError::InvalidExportContext);
    }
    inventory
        .validate_against_export(session.session_layout(), offer)
        .map_err(|_| GenerationBaselineError::InvalidExportInventory)?;

    if inventory.scope_sources().len() != session.session_layout().entries().len() {
        return Err(GenerationBaselineError::ScopeAuthorityMismatch);
    }
    let offered_sources = offer
        .offer()
        .source_generations
        .iter()
        .map(|generation| &generation.source_connection_id)
        .collect::<BTreeSet<_>>();
    let mut mount_source_pairs = BTreeSet::new();
    let mut session_mounts = BTreeSet::new();
    for (layout_entry, authority) in session
        .session_layout()
        .entries()
        .iter()
        .zip(inventory.scope_sources())
    {
        if layout_entry.scope_ordinal() != authority.scope_ordinal()
            || !offered_sources.contains(authority.source_connection_id())
        {
            return Err(GenerationBaselineError::ScopeAuthorityMismatch);
        }
        session_mounts.insert(layout_entry.mount_id().clone());
        mount_source_pairs.insert((
            layout_entry.mount_id().clone(),
            authority.source_connection_id().clone(),
        ));
    }
    let inventory_mounts = inventory
        .target_directories()
        .iter()
        .map(|target| target.mount_id().clone())
        .collect::<BTreeSet<_>>();
    if inventory_mounts != session_mounts {
        return Err(GenerationBaselineError::MountSetMismatch);
    }

    Ok(session_mounts
        .into_iter()
        .map(|mount_id| {
            let source_connection_ids = offer
                .offer()
                .source_generations
                .iter()
                .filter(|generation| {
                    mount_source_pairs
                        .contains(&(mount_id.clone(), generation.source_connection_id.clone()))
                })
                .map(|generation| generation.source_connection_id.clone())
                .collect();
            ExpectedMountSources {
                mount_id,
                source_connection_ids,
            }
        })
        .collect())
}

fn validate_baseline_files(
    files: &[GenerationFileIdentity],
) -> Result<(), GenerationBaselineError> {
    let mut previous_projection_id = None;
    let mut path_collision_keys = BTreeSet::new();
    for file in files {
        validate_nonempty("projection_id", file.projection_id.as_str())?;
        validate_nonempty("content_version_id", file.content_version_id.as_str())?;
        if file.content_version_id.as_str().len() > MAX_GENERATION_BASELINE_CONTENT_VERSION_ID_BYTES
        {
            return Err(GenerationBaselineError::ContentVersionIdTooLong {
                actual: file.content_version_id.as_str().len(),
            });
        }
        validate_sha256("content_sha256", &file.content_sha256)?;
        if previous_projection_id
            .is_some_and(|previous: &ProjectionId| previous >= &file.projection_id)
        {
            return Err(GenerationBaselineError::NonCanonicalFileOrder);
        }
        previous_projection_id = Some(&file.projection_id);
        if !path_collision_keys.insert(file.logical_path.portable_collision_key()) {
            return Err(GenerationBaselineError::FilePathReuse);
        }
    }
    Ok(())
}

fn baseline_target_inventory_sha256(
    files: &[GenerationFileIdentity],
) -> Result<String, GenerationBaselineError> {
    validate_baseline_files(files)?;
    let mut output = GENERATION_TARGET_INVENTORY_V1_DOMAIN.to_vec();
    append_count(&mut output, files.len())?;
    for file in files {
        append_text(&mut output, file.projection_id.as_str())?;
        append_text(&mut output, file.logical_path.as_str())?;
        append_text(&mut output, file.content_version_id.as_str())?;
        append_text(&mut output, &file.content_sha256)?;
        append_u64(&mut output, file.byte_length);
    }
    Ok(sha256_label(&output))
}

fn refresh_mode_for_files(files: &[GenerationFileIdentity]) -> GenerationBaselineRefreshModeV1 {
    if files.iter().all(|file| {
        file.projection_id.as_str().len() <= MAX_DELIVERY_ID_BYTES
            && file.content_version_id.as_str().len() <= MAX_DELIVERY_ID_BYTES
            && file.byte_length <= MAX_GENERATION_FILE_BYTES
    }) {
        GenerationBaselineRefreshModeV1::GenerationDeltaV1
    } else {
        GenerationBaselineRefreshModeV1::FullExportOnly
    }
}

fn refresh_mode_for_source(
    mount_id: &PortableMountId,
    source: &GenerationBaselineSourceV1,
) -> GenerationBaselineRefreshModeV1 {
    if mount_id.as_str().len() <= MAX_DELIVERY_ID_BYTES
        && refresh_mode_for_source_fields(
            &source.source_connection_id,
            &source.observed_generation_id,
            &source.files,
        ) == GenerationBaselineRefreshModeV1::GenerationDeltaV1
    {
        GenerationBaselineRefreshModeV1::GenerationDeltaV1
    } else {
        GenerationBaselineRefreshModeV1::FullExportOnly
    }
}

fn refresh_mode_for_source_fields(
    source_connection_id: &SourceConnectionId,
    observed_generation_id: &SourceGenerationId,
    files: &[GenerationFileIdentity],
) -> GenerationBaselineRefreshModeV1 {
    if source_connection_id.as_str().len() <= MAX_DELIVERY_ID_BYTES
        && observed_generation_id.as_str().len() <= MAX_DELIVERY_ID_BYTES
        && refresh_mode_for_files(files) == GenerationBaselineRefreshModeV1::GenerationDeltaV1
    {
        GenerationBaselineRefreshModeV1::GenerationDeltaV1
    } else {
        GenerationBaselineRefreshModeV1::FullExportOnly
    }
}

fn refresh_mode_label(mode: GenerationBaselineRefreshModeV1) -> &'static str {
    match mode {
        GenerationBaselineRefreshModeV1::GenerationDeltaV1 => "generation_delta_v1",
        GenerationBaselineRefreshModeV1::FullExportOnly => "full_export_only",
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
    sources: Vec<GenerationBaselineSourceV1Wire>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationBaselineSourceV1Wire {
    source_connection_id: SourceConnectionId,
    observed_generation_id: SourceGenerationId,
    target_inventory_sha256: String,
    refresh_mode: GenerationBaselineRefreshModeV1,
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
                    sources: mount
                        .sources
                        .into_iter()
                        .map(|source| GenerationBaselineSourceV1 {
                            source_connection_id: source.source_connection_id,
                            observed_generation_id: source.observed_generation_id,
                            target_inventory_sha256: source.target_inventory_sha256,
                            refresh_mode: source.refresh_mode,
                            files: source
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
    UpdateRequired {
        minimum: u16,
        supported: u16,
    },
    UnsupportedFormatVersion {
        actual: u16,
    },
    InvalidVersionEnvelope,
    ZeroProfileRevision,
    UnsupportedLayoutVersion {
        actual: u16,
    },
    IdentifierEmpty(&'static str),
    InvalidSha256(&'static str),
    ContentVersionIdTooLong {
        actual: usize,
    },
    SourceGenerationCount {
        actual: usize,
    },
    NonCanonicalSourceGenerationOrder {
        index: usize,
        actual: u32,
    },
    DuplicateSourceConnection,
    DuplicateSourceGeneration,
    MountCount {
        actual: usize,
    },
    MountSourceCount {
        mount_id: String,
        actual: usize,
    },
    SourceStateCount {
        actual: usize,
    },
    NonCanonicalMountOrder {
        mount_index: usize,
    },
    NonCanonicalMountSourceOrder {
        mount_index: usize,
        source_index: usize,
    },
    MountSourceNotInGenerationVector {
        mount_id: String,
    },
    MountGenerationMismatch {
        mount_id: String,
        source_connection_id: String,
    },
    SourceSetMismatch,
    TargetInventoryMismatch {
        mount_id: String,
        source_connection_id: String,
    },
    RefreshModeMismatch {
        mount_id: String,
        source_connection_id: String,
    },
    NonCanonicalFileOrder,
    FilePathReuse,
    DuplicateProjectionId {
        projection_id: String,
    },
    ContentLengthOverflow,
    CanonicalValueTooLarge,
    BaselineDigestMismatch,
    InvalidExportContext,
    InvalidExportInventory,
    ScopeAuthorityMismatch,
    MountSetMismatch,
    MountSourceSetMismatch {
        mount_id: String,
    },
    InventoryFileMismatch {
        projection_id: String,
    },
    InventoryFilesMissing,
    EncodedLimitOverflow,
    EncodingTooLarge {
        actual: usize,
        maximum: usize,
    },
    ExportBindingMismatch,
    InvalidJson(String),
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
            Self::InvalidSha256(field) => write!(
                formatter,
                "{field} must be `sha256:` plus 64 lowercase hexadecimal digits"
            ),
            Self::ContentVersionIdTooLong { actual } => write!(
                formatter,
                "content version ID is {actual} bytes, exceeding {MAX_GENERATION_BASELINE_CONTENT_VERSION_ID_BYTES}"
            ),
            Self::SourceGenerationCount { actual } => write!(
                formatter,
                "generation baseline has {actual} source generations; expected 1 through {MAX_PROFILE_SCOPE_BINDINGS}"
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
                "generation baseline has {actual} mounts; expected 1 through {MAX_PROFILE_MOUNTS}"
            ),
            Self::MountSourceCount { mount_id, actual } => write!(
                formatter,
                "mount `{mount_id}` has {actual} source states; expected 1 through {MAX_PROFILE_SCOPE_BINDINGS}"
            ),
            Self::SourceStateCount { actual } => write!(
                formatter,
                "generation baseline has {actual} source states, exceeding {MAX_PROFILE_SCOPE_BINDINGS}"
            ),
            Self::NonCanonicalMountOrder { mount_index } => write!(
                formatter,
                "mount at index {mount_index} is not in exact mount-ID byte order"
            ),
            Self::NonCanonicalMountSourceOrder { mount_index, source_index } => write!(
                formatter,
                "source state {source_index} in mount {mount_index} is not in source-generation order"
            ),
            Self::MountSourceNotInGenerationVector { mount_id } => write!(
                formatter,
                "mount `{mount_id}` references a source absent from the generation vector"
            ),
            Self::MountGenerationMismatch {
                mount_id,
                source_connection_id,
            } => write!(
                formatter,
                "mount `{mount_id}` source `{source_connection_id}` observed a generation different from its source vector entry"
            ),
            Self::SourceSetMismatch => formatter.write_str(
                "mount sources do not exactly cover the source-generation vector",
            ),
            Self::TargetInventoryMismatch {
                mount_id,
                source_connection_id,
            } => write!(
                formatter,
                "mount `{mount_id}` source `{source_connection_id}` target inventory digest does not match its files"
            ),
            Self::RefreshModeMismatch {
                mount_id,
                source_connection_id,
            } => write!(
                formatter,
                "mount `{mount_id}` source `{source_connection_id}` declares the wrong refresh mode"
            ),
            Self::NonCanonicalFileOrder => {
                formatter.write_str("baseline files are not in canonical projection-ID order")
            }
            Self::FilePathReuse => formatter.write_str("baseline files reuse a logical path"),
            Self::DuplicateProjectionId { projection_id } => write!(
                formatter,
                "projection ID `{projection_id}` occurs in more than one baseline source state"
            ),
            Self::ContentLengthOverflow => formatter.write_str("content byte total overflow"),
            Self::CanonicalValueTooLarge => {
                formatter.write_str("value is too large for canonical encoding")
            }
            Self::BaselineDigestMismatch => {
                formatter.write_str("generation baseline digest does not match its canonical preimage")
            }
            Self::InvalidExportContext => {
                formatter.write_str("session and export offer bindings are inconsistent")
            }
            Self::InvalidExportInventory => {
                formatter.write_str("export inventory is not canonical for the session and offer")
            }
            Self::ScopeAuthorityMismatch => formatter.write_str(
                "session layout and export scope-source authority do not match",
            ),
            Self::MountSetMismatch => {
                formatter.write_str("baseline mount set does not match the export layout")
            }
            Self::MountSourceSetMismatch { mount_id } => write!(
                formatter,
                "mount `{mount_id}` source states do not match export scope authority"
            ),
            Self::InventoryFileMismatch { projection_id } => write!(
                formatter,
                "baseline projection `{projection_id}` does not match its authoritative export inventory record"
            ),
            Self::InventoryFilesMissing => {
                formatter.write_str("baseline omits authoritative export inventory files")
            }
            Self::EncodedLimitOverflow => {
                formatter.write_str("negotiated baseline encoding ceiling overflow")
            }
            Self::EncodingTooLarge { actual, maximum } => write!(
                formatter,
                "generation baseline encoding is {actual} bytes, exceeding negotiated ceiling {maximum}"
            ),
            Self::ExportBindingMismatch => formatter.write_str(
                "generation baseline does not match the exact profile, session, layout, inventory, attempt, and generation vector",
            ),
            Self::InvalidJson(error) => write!(formatter, "invalid generation baseline JSON: {error}"),
        }
    }
}

impl std::error::Error for GenerationBaselineError {}

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

fn validate_nonempty(field: &'static str, value: &str) -> Result<(), GenerationBaselineError> {
    if value.is_empty() {
        Err(GenerationBaselineError::IdentifierEmpty(field))
    } else {
        Ok(())
    }
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

fn add_escaped_bytes(total: &mut usize, length: usize) -> Result<(), GenerationBaselineError> {
    *total = total
        .checked_add(
            length
                .checked_mul(MAX_JSON_ESCAPE_EXPANSION)
                .ok_or(GenerationBaselineError::EncodedLimitOverflow)?,
        )
        .ok_or(GenerationBaselineError::EncodedLimitOverflow)?;
    Ok(())
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
