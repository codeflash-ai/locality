//! Pure generation-2 workspace export inventory and terminal-control contracts.
//!
//! This module maps connector-owned logical paths through a sealed session
//! layout. It deliberately returns portable relative paths only: binding the
//! resulting plan to a staging or publication root remains host work.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};

use caseless::Caseless;
use locality_core::portable::{
    LogicalPath, ProjectionFileKind, ProjectionId, SourceAction, SourceConnectionId,
};
use locality_core::workspace_layout::{MountTarget, PortableMountId};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization_v16::UnicodeNormalization;

use crate::workspace_api_v2::{
    REQUIRED_MAX_COMPONENT_UTF8_BYTES, REQUIRED_MAX_COMPONENT_UTF16_UNITS,
    REQUIRED_MAX_PATH_UTF8_BYTES, REQUIRED_MAX_PATH_UTF16_UNITS, WORKSPACE_HTTP_API_GENERATION_V2,
    WorkspaceExportOfferV2, WorkspaceProfileSessionV2,
};
use crate::workspace_layout::{LayoutDigest, SessionLayout, WorkspaceProfileId};
use crate::{
    ExportCompletionReceipt, MAX_EXPORT_TERMINAL_CONTROL_BYTES, RESERVED_EXPORT_METADATA_PATH,
    ScopeAuthorizedWritableExportMetadata, projection_file_kind_wire_label,
    source_action_wire_label,
};

pub const WORKSPACE_EXPORT_INVENTORY_V2_DOMAIN: &[u8] = b"locality.workspace-export.inventory.v2\0";
pub const WORKSPACE_EXPORT_INVENTORY_V2_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/workspace-export-inventory-v2.json");
pub const WORKSPACE_EXPORT_TERMINAL_CONTROL_V2_GOLDEN_JSON: &[u8] =
    include_bytes!("../fixtures/workspace-export-terminal-control-v2.json");

const _: () = assert!(unicode_normalization_v16::UNICODE_VERSION.0 == 16);
const _: () = assert!(unicode_normalization_v16::UNICODE_VERSION.1 == 0);
const _: () = assert!(unicode_normalization_v16::UNICODE_VERSION.2 == 0);
const _: () = assert!(caseless::UNICODE_VERSION.0 == 16);
const _: () = assert!(caseless::UNICODE_VERSION.1 == 0);
const _: () = assert!(caseless::UNICODE_VERSION.2 == 0);

/// One directory or file selected by the authorized query before a workspace
/// target is applied. `logical_path` is connector-owned and is never rewritten.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "record_class", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceAuthorizedExportEntryV2 {
    Directory {
        winning_scope_ordinal: u32,
        mount_id: PortableMountId,
        logical_path: String,
    },
    File {
        winning_scope_ordinal: u32,
        mount_id: PortableMountId,
        logical_path: String,
        projection_id: ProjectionId,
        source_connection_id: SourceConnectionId,
        file_kind: ProjectionFileKind,
        effective_actions: BTreeSet<SourceAction>,
        content_sha256: String,
        byte_length: u64,
    },
}

impl WorkspaceAuthorizedExportEntryV2 {
    fn scope_ordinal(&self) -> u32 {
        match self {
            Self::Directory {
                winning_scope_ordinal,
                ..
            }
            | Self::File {
                winning_scope_ordinal,
                ..
            } => *winning_scope_ordinal,
        }
    }

    fn mount_id(&self) -> &PortableMountId {
        match self {
            Self::Directory { mount_id, .. } | Self::File { mount_id, .. } => mount_id,
        }
    }

    fn logical_path(&self) -> &str {
        match self {
            Self::Directory { logical_path, .. } | Self::File { logical_path, .. } => logical_path,
        }
    }
}

/// A canonical archive record after applying the sealed target mapping.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "record_class", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceNamespacedExportRecordV2 {
    TargetDirectory {
        mount_id: PortableMountId,
        target: MountTarget,
        member_path: String,
    },
    Directory {
        winning_scope_ordinal: u32,
        mount_id: PortableMountId,
        target: MountTarget,
        logical_path: LogicalPath,
        member_path: String,
    },
    File {
        winning_scope_ordinal: u32,
        mount_id: PortableMountId,
        target: MountTarget,
        logical_path: LogicalPath,
        member_path: String,
        projection_id: ProjectionId,
        source_connection_id: SourceConnectionId,
        file_kind: ProjectionFileKind,
        effective_actions: BTreeSet<SourceAction>,
        content_sha256: String,
        byte_length: u64,
    },
    Control {
        member_path: String,
    },
}

impl WorkspaceNamespacedExportRecordV2 {
    pub fn member_path(&self) -> &str {
        match self {
            Self::TargetDirectory { member_path, .. }
            | Self::Directory { member_path, .. }
            | Self::File { member_path, .. }
            | Self::Control { member_path } => member_path,
        }
    }
}

/// Sealed authority for one session-layout scope ordinal. The complete vector
/// is canonical scope order and its source set must exactly match the export
/// offer's source-generation vector.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceScopeSourceAuthorityV2 {
    scope_ordinal: u32,
    source_connection_id: SourceConnectionId,
}

impl WorkspaceScopeSourceAuthorityV2 {
    pub fn new(scope_ordinal: u32, source_connection_id: SourceConnectionId) -> Self {
        Self {
            scope_ordinal,
            source_connection_id,
        }
    }

    pub fn scope_ordinal(&self) -> u32 {
        self.scope_ordinal
    }

    pub fn source_connection_id(&self) -> &SourceConnectionId {
        &self.source_connection_id
    }
}

/// Counts for one explicit authorized target root. Directory counts include
/// the target root itself, which means an empty target has `directory_count=1`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceTargetDirectoryV2 {
    mount_id: PortableMountId,
    target: MountTarget,
    directory_count: u64,
    file_count: u64,
    content_bytes: u64,
}

impl WorkspaceTargetDirectoryV2 {
    pub fn mount_id(&self) -> &PortableMountId {
        &self.mount_id
    }

    pub fn target(&self) -> &MountTarget {
        &self.target
    }

    pub fn directory_count(&self) -> u64 {
        self.directory_count
    }

    pub fn file_count(&self) -> u64 {
        self.file_count
    }

    pub fn content_bytes(&self) -> u64 {
        self.content_bytes
    }
}

/// Complete deterministic generation-2 inventory. It contains no host root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceNamespacedInventoryV2 {
    scope_sources: Vec<WorkspaceScopeSourceAuthorityV2>,
    target_directories: Vec<WorkspaceTargetDirectoryV2>,
    records: Vec<WorkspaceNamespacedExportRecordV2>,
    control_entry_count: u64,
    file_count: u64,
    directory_count: u64,
    archive_entry_count: u64,
    selected_content_bytes: u64,
    inventory_sha256: String,
}

impl WorkspaceNamespacedInventoryV2 {
    pub fn plan(
        session_layout: &SessionLayout,
        offer: &WorkspaceExportOfferV2,
        scope_sources: &[WorkspaceScopeSourceAuthorityV2],
        entries: &[WorkspaceAuthorizedExportEntryV2],
    ) -> Result<Self, WorkspaceExportV2Error> {
        plan_inventory(session_layout, offer, scope_sources, entries)
    }

    /// Decode and recompute the complete inventory from its authorized
    /// directory/file records, sealed session layout, and sealed offer. The
    /// aggregate intentionally has no public unchecked `Deserialize` path.
    pub fn decode_json(
        input: &[u8],
        session_layout: &SessionLayout,
        offer: &WorkspaceExportOfferV2,
    ) -> Result<Self, WorkspaceExportV2Error> {
        let wire: WorkspaceNamespacedInventoryV2Wire = serde_json::from_slice(input)
            .map_err(|error| WorkspaceExportV2Error::InvalidJson(error.to_string()))?;
        let decoded = wire.into_inventory();
        let entries = decoded
            .records
            .iter()
            .filter_map(authorized_entry_from_record)
            .collect::<Vec<_>>();
        let expected = Self::plan(session_layout, offer, &decoded.scope_sources, &entries)?;
        expected.validate_against_offer(offer)?;
        if decoded != expected {
            return Err(WorkspaceExportV2Error::ArchiveDoesNotMatchInventory);
        }
        Ok(decoded)
    }

    pub fn scope_sources(&self) -> &[WorkspaceScopeSourceAuthorityV2] {
        &self.scope_sources
    }

    pub fn target_directories(&self) -> &[WorkspaceTargetDirectoryV2] {
        &self.target_directories
    }

    pub fn records(&self) -> &[WorkspaceNamespacedExportRecordV2] {
        &self.records
    }

    pub fn control_entry_count(&self) -> u64 {
        self.control_entry_count
    }

    pub fn file_count(&self) -> u64 {
        self.file_count
    }

    pub fn directory_count(&self) -> u64 {
        self.directory_count
    }

    pub fn archive_entry_count(&self) -> u64 {
        self.archive_entry_count
    }

    pub fn selected_content_bytes(&self) -> u64 {
        self.selected_content_bytes
    }

    pub fn inventory_sha256(&self) -> &str {
        &self.inventory_sha256
    }

    pub fn canonical_preimage(&self) -> Result<Vec<u8>, WorkspaceExportV2Error> {
        canonical_inventory_preimage(&self.scope_sources, &self.records)
    }

    pub fn validate_against_offer(
        &self,
        offer: &WorkspaceExportOfferV2,
    ) -> Result<(), WorkspaceExportV2Error> {
        offer.validate()?;
        let sealed = offer.offer();
        if self.control_entry_count != sealed.control_entry_count
            || self.file_count != sealed.file_count
            || self.directory_count != sealed.directory_count
            || self.archive_entry_count != sealed.archive_entry_count
            || self.selected_content_bytes != sealed.selected_content_bytes
            || self.inventory_sha256 != sealed.inventory_sha256
        {
            return Err(WorkspaceExportV2Error::InventoryDoesNotMatchOffer);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceNamespacedInventoryV2Wire {
    scope_sources: Vec<WorkspaceScopeSourceAuthorityV2>,
    target_directories: Vec<WorkspaceTargetDirectoryV2>,
    records: Vec<WorkspaceNamespacedExportRecordV2>,
    control_entry_count: u64,
    file_count: u64,
    directory_count: u64,
    archive_entry_count: u64,
    selected_content_bytes: u64,
    inventory_sha256: String,
}

impl WorkspaceNamespacedInventoryV2Wire {
    fn into_inventory(self) -> WorkspaceNamespacedInventoryV2 {
        WorkspaceNamespacedInventoryV2 {
            scope_sources: self.scope_sources,
            target_directories: self.target_directories,
            records: self.records,
            control_entry_count: self.control_entry_count,
            file_count: self.file_count,
            directory_count: self.directory_count,
            archive_entry_count: self.archive_entry_count,
            selected_content_bytes: self.selected_content_bytes,
            inventory_sha256: self.inventory_sha256,
        }
    }
}

/// Layout, offer, inventory, target, and count facts declared before the
/// terminal receipt is accepted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceExportControlMetadataV2 {
    api_generation: u16,
    profile_id: WorkspaceProfileId,
    profile_revision: u64,
    layout_version: u16,
    layout_digest: LayoutDigest,
    session_id: locality_core::portable::SessionId,
    export_attempt_id: locality_core::portable::ExportAttemptId,
    inventory_sha256: String,
    scope_sources: Vec<WorkspaceScopeSourceAuthorityV2>,
    target_directories: Vec<WorkspaceTargetDirectoryV2>,
    declared_control_entry_count: u64,
    declared_file_count: u64,
    declared_directory_count: u64,
    declared_archive_entry_count: u64,
    declared_content_bytes: u64,
}

impl WorkspaceExportControlMetadataV2 {
    pub fn new(
        session: &WorkspaceProfileSessionV2,
        offer: &WorkspaceExportOfferV2,
        inventory: &WorkspaceNamespacedInventoryV2,
    ) -> Result<Self, WorkspaceExportV2Error> {
        let metadata = Self {
            api_generation: WORKSPACE_HTTP_API_GENERATION_V2,
            profile_id: session.profile_id().clone(),
            profile_revision: session.profile_revision(),
            layout_version: session.session_layout().layout_version(),
            layout_digest: session.session_layout().layout_digest().clone(),
            session_id: session.session_id().clone(),
            export_attempt_id: offer.offer().export_attempt_id.clone(),
            inventory_sha256: inventory.inventory_sha256.clone(),
            scope_sources: inventory.scope_sources.clone(),
            target_directories: inventory.target_directories.clone(),
            declared_control_entry_count: inventory.control_entry_count,
            declared_file_count: inventory.file_count,
            declared_directory_count: inventory.directory_count,
            declared_archive_entry_count: inventory.archive_entry_count,
            declared_content_bytes: inventory.selected_content_bytes,
        };
        metadata.validate_against(session, offer, inventory)?;
        Ok(metadata)
    }

    pub fn layout_digest(&self) -> &LayoutDigest {
        &self.layout_digest
    }

    pub fn inventory_sha256(&self) -> &str {
        &self.inventory_sha256
    }

    pub fn scope_sources(&self) -> &[WorkspaceScopeSourceAuthorityV2] {
        &self.scope_sources
    }

    pub fn target_directories(&self) -> &[WorkspaceTargetDirectoryV2] {
        &self.target_directories
    }

    fn validate_shape(&self) -> Result<(), WorkspaceExportV2Error> {
        if self.api_generation != WORKSPACE_HTTP_API_GENERATION_V2 {
            return Err(WorkspaceExportV2Error::UnsupportedApiGeneration {
                actual: self.api_generation,
            });
        }
        if self.profile_revision == 0
            || self.layout_version != crate::workspace_layout::WORKSPACE_LAYOUT_VERSION
        {
            return Err(WorkspaceExportV2Error::ControlBindingMismatch);
        }
        validate_sha256(&self.inventory_sha256)?;
        validate_scope_source_shape(&self.scope_sources)?;
        validate_target_declarations(&self.target_directories)?;
        let (directories, files, bytes) = sum_target_counts(&self.target_directories)?;
        let archive_entries = directories
            .checked_add(files)
            .and_then(|count| count.checked_add(self.declared_control_entry_count))
            .ok_or(WorkspaceExportV2Error::CountOverflow)?;
        if self.declared_control_entry_count != 1
            || directories != self.declared_directory_count
            || files != self.declared_file_count
            || bytes != self.declared_content_bytes
            || archive_entries != self.declared_archive_entry_count
        {
            return Err(WorkspaceExportV2Error::DeclaredCountsMismatch);
        }
        Ok(())
    }

    fn validate_against(
        &self,
        session: &WorkspaceProfileSessionV2,
        offer: &WorkspaceExportOfferV2,
        inventory: &WorkspaceNamespacedInventoryV2,
    ) -> Result<(), WorkspaceExportV2Error> {
        self.validate_shape()?;
        session.validate()?;
        inventory.validate_against_offer(offer)?;
        if self.profile_id != *session.profile_id()
            || self.profile_revision != session.profile_revision()
            || self.layout_version != session.session_layout().layout_version()
            || self.layout_digest != *session.session_layout().layout_digest()
            || self.profile_id != *offer.profile_id()
            || self.profile_revision != offer.profile_revision()
            || self.layout_version != offer.layout_version()
            || self.layout_digest != *offer.layout_digest()
            || self.session_id != *session.session_id()
            || self.session_id != offer.offer().session_id
            || self.export_attempt_id != offer.offer().export_attempt_id
            || self.inventory_sha256 != offer.offer().inventory_sha256
            || self.scope_sources != inventory.scope_sources
            || self.target_directories != inventory.target_directories
            || self.declared_control_entry_count != inventory.control_entry_count
            || self.declared_file_count != inventory.file_count
            || self.declared_directory_count != inventory.directory_count
            || self.declared_archive_entry_count != inventory.archive_entry_count
            || self.declared_content_bytes != inventory.selected_content_bytes
        {
            return Err(WorkspaceExportV2Error::ControlBindingMismatch);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceExportControlMetadataV2Wire {
    api_generation: u16,
    profile_id: WorkspaceProfileId,
    profile_revision: u64,
    layout_version: u16,
    layout_digest: LayoutDigest,
    session_id: locality_core::portable::SessionId,
    export_attempt_id: locality_core::portable::ExportAttemptId,
    inventory_sha256: String,
    scope_sources: Vec<WorkspaceScopeSourceAuthorityV2>,
    target_directories: Vec<WorkspaceTargetDirectoryV2>,
    declared_control_entry_count: u64,
    declared_file_count: u64,
    declared_directory_count: u64,
    declared_archive_entry_count: u64,
    declared_content_bytes: u64,
}

impl<'de> Deserialize<'de> for WorkspaceExportControlMetadataV2 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkspaceExportControlMetadataV2Wire::deserialize(deserializer)?;
        let metadata = Self {
            api_generation: wire.api_generation,
            profile_id: wire.profile_id,
            profile_revision: wire.profile_revision,
            layout_version: wire.layout_version,
            layout_digest: wire.layout_digest,
            session_id: wire.session_id,
            export_attempt_id: wire.export_attempt_id,
            inventory_sha256: wire.inventory_sha256,
            scope_sources: wire.scope_sources,
            target_directories: wire.target_directories,
            declared_control_entry_count: wire.declared_control_entry_count,
            declared_file_count: wire.declared_file_count,
            declared_directory_count: wire.declared_directory_count,
            declared_archive_entry_count: wire.declared_archive_entry_count,
            declared_content_bytes: wire.declared_content_bytes,
        };
        metadata
            .validate_shape()
            .map_err(serde::de::Error::custom)?;
        Ok(metadata)
    }
}

/// Generation-2 receipt. The embedded v1-compatible delivery receipt remains
/// untouched while the outer metadata binds it to the workspace namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceExportCompletionReceiptV2 {
    pub metadata: WorkspaceExportControlMetadataV2,
    pub receipt: ExportCompletionReceipt,
}

impl WorkspaceExportCompletionReceiptV2 {
    pub fn validate_against(
        &self,
        session: &WorkspaceProfileSessionV2,
        offer: &WorkspaceExportOfferV2,
        inventory: &WorkspaceNamespacedInventoryV2,
    ) -> Result<(), WorkspaceExportV2Error> {
        self.metadata.validate_against(session, offer, inventory)?;
        self.receipt.validate_against(offer.offer())?;
        if self.receipt.versions != offer.offer().versions
            || self.receipt.session_id != self.metadata.session_id
            || self.receipt.export_attempt_id != self.metadata.export_attempt_id
            || self.receipt.inventory_sha256 != self.metadata.inventory_sha256
            || self.receipt.delivered_control_entry_count
                != self.metadata.declared_control_entry_count
            || self.receipt.delivered_file_count != self.metadata.declared_file_count
            || self.receipt.delivered_directory_count != self.metadata.declared_directory_count
            || self.receipt.delivered_archive_entry_count
                != self.metadata.declared_archive_entry_count
            || self.receipt.delivered_content_bytes != self.metadata.declared_content_bytes
        {
            return Err(WorkspaceExportV2Error::ReceiptBindingMismatch);
        }
        Ok(())
    }
}

/// Exact payload of the single final generation-2 control member.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceExportTerminalControlV2 {
    pub metadata: WorkspaceExportControlMetadataV2,
    pub writable_metadata: ScopeAuthorizedWritableExportMetadata,
    pub completion_receipt: WorkspaceExportCompletionReceiptV2,
}

impl WorkspaceExportTerminalControlV2 {
    pub fn decode_json(
        input: &[u8],
        session: &WorkspaceProfileSessionV2,
        offer: &WorkspaceExportOfferV2,
        inventory: &WorkspaceNamespacedInventoryV2,
    ) -> Result<Self, WorkspaceExportV2Error> {
        if input.len() > MAX_EXPORT_TERMINAL_CONTROL_BYTES {
            return Err(WorkspaceExportV2Error::ControlTooLarge {
                actual: input.len(),
            });
        }
        let control: Self = serde_json::from_slice(input)
            .map_err(|error| WorkspaceExportV2Error::InvalidJson(error.to_string()))?;
        let canonical = serde_json::to_vec(&control)
            .map_err(|error| WorkspaceExportV2Error::InvalidJson(error.to_string()))?;
        if input != canonical {
            return Err(WorkspaceExportV2Error::NonCanonicalControlJson);
        }
        control.validate_against(session, offer, inventory)?;
        Ok(control)
    }

    pub fn validate_against(
        &self,
        session: &WorkspaceProfileSessionV2,
        offer: &WorkspaceExportOfferV2,
        inventory: &WorkspaceNamespacedInventoryV2,
    ) -> Result<(), WorkspaceExportV2Error> {
        self.metadata.validate_against(session, offer, inventory)?;
        if self.completion_receipt.metadata != self.metadata {
            return Err(WorkspaceExportV2Error::ReceiptBindingMismatch);
        }
        self.completion_receipt
            .validate_against(session, offer, inventory)?;
        self.writable_metadata.validate_against(offer.offer())?;
        if self.writable_metadata.versions != offer.offer().versions {
            return Err(WorkspaceExportV2Error::ControlBindingMismatch);
        }
        validate_writable_entries(&self.writable_metadata, inventory)?;
        Ok(())
    }
}

/// Archive entry kinds accepted by the pure client planner. Link and device
/// variants exist only so adapters can fail closed before host filesystem I/O.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceArchiveEntryKindV2 {
    Directory,
    File,
    Control,
    Symlink,
    Hardlink,
    BlockDevice,
    CharacterDevice,
    Fifo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceArchiveMemberV2 {
    pub kind: WorkspaceArchiveEntryKindV2,
    pub member_path: String,
    pub authorized_entry: Option<WorkspaceAuthorizedExportEntryV2>,
}

/// One host-neutral mapping operation. No absolute staging/publication root is
/// accepted or returned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMaterializationEntryV2 {
    pub kind: WorkspaceArchiveEntryKindV2,
    pub mount_id: PortableMountId,
    pub target: MountTarget,
    pub logical_path: Option<LogicalPath>,
    pub member_path: String,
    pub projection_id: Option<ProjectionId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceMaterializationPlanV2 {
    target_directories: Vec<WorkspaceTargetDirectoryV2>,
    entries: Vec<WorkspaceMaterializationEntryV2>,
}

impl WorkspaceMaterializationPlanV2 {
    pub fn plan(
        session: &WorkspaceProfileSessionV2,
        offer: &WorkspaceExportOfferV2,
        control: &WorkspaceExportTerminalControlV2,
        members: &[WorkspaceArchiveMemberV2],
    ) -> Result<Self, WorkspaceExportV2Error> {
        validate_control_sequence(members)?;

        let declared_targets = session
            .session_layout()
            .entries()
            .iter()
            .map(|entry| entry.target().as_str())
            .collect::<BTreeSet<_>>();
        let mut authorized_entries = Vec::new();
        for member in members {
            match member.kind {
                WorkspaceArchiveEntryKindV2::Directory | WorkspaceArchiveEntryKindV2::File => {
                    if member.kind == WorkspaceArchiveEntryKindV2::Directory
                        && member.authorized_entry.is_none()
                        && declared_targets.contains(member.member_path.as_str())
                    {
                        continue;
                    }
                    let entry = member
                        .authorized_entry
                        .as_ref()
                        .ok_or(WorkspaceExportV2Error::MissingAuthorizedEntry)?;
                    let kind_matches = matches!(
                        (member.kind, entry),
                        (
                            WorkspaceArchiveEntryKindV2::Directory,
                            WorkspaceAuthorizedExportEntryV2::Directory { .. }
                        ) | (
                            WorkspaceArchiveEntryKindV2::File,
                            WorkspaceAuthorizedExportEntryV2::File { .. }
                        )
                    );
                    if !kind_matches {
                        return Err(WorkspaceExportV2Error::EntryKindMismatch);
                    }
                    authorized_entries.push(entry.clone());
                }
                WorkspaceArchiveEntryKindV2::Control => {
                    if member.authorized_entry.is_some() {
                        return Err(WorkspaceExportV2Error::EntryKindMismatch);
                    }
                }
                unsupported => {
                    return Err(WorkspaceExportV2Error::UnsupportedArchiveEntryKind {
                        kind: unsupported,
                    });
                }
            }
        }

        let inventory = WorkspaceNamespacedInventoryV2::plan(
            session.session_layout(),
            offer,
            control.metadata.scope_sources(),
            &authorized_entries,
        )?;
        control.validate_against(session, offer, &inventory)?;
        if members.len() != inventory.records.len() {
            return Err(WorkspaceExportV2Error::ArchiveDoesNotMatchInventory);
        }

        let targets = inventory
            .target_directories
            .iter()
            .map(|target| target.target.as_str())
            .collect::<BTreeSet<_>>();
        let mut seen_paths = BTreeSet::new();
        let mut collision_keys = BTreeSet::new();
        for (member, expected) in members.iter().zip(&inventory.records) {
            validate_stream_member_path(member, &targets)?;
            if !seen_paths.insert(member.member_path.as_str()) {
                return Err(WorkspaceExportV2Error::DuplicateMaterializedPath {
                    path: member.member_path.clone(),
                });
            }
            if !collision_keys.insert(collision_key(&member.member_path)) {
                return Err(WorkspaceExportV2Error::CaseFoldCollision {
                    path: member.member_path.clone(),
                });
            }
            if member.member_path != expected.member_path() {
                return Err(WorkspaceExportV2Error::ArchiveDoesNotMatchInventory);
            }
        }

        let entries = inventory
            .records
            .iter()
            .filter_map(materialization_entry)
            .collect();
        Ok(Self {
            target_directories: inventory.target_directories,
            entries,
        })
    }

    pub fn target_directories(&self) -> &[WorkspaceTargetDirectoryV2] {
        &self.target_directories
    }

    pub fn entries(&self) -> &[WorkspaceMaterializationEntryV2] {
        &self.entries
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceExportV2Error {
    UnsupportedApiGeneration {
        actual: u16,
    },
    UnknownScopeOrdinal {
        actual: u32,
    },
    UnknownMount {
        scope_ordinal: u32,
        mount_id: String,
    },
    InvalidScopeSourceAuthority,
    SourceNotInOffer {
        source_connection_id: String,
    },
    SourceScopeMismatch {
        scope_ordinal: u32,
        expected: String,
        actual: String,
    },
    UnknownTopLevelTarget {
        target: String,
    },
    InvalidLogicalPath {
        path: String,
    },
    InvalidComponent {
        component: String,
    },
    NonNfcComponent {
        component: String,
    },
    ComponentUtf8TooLong {
        actual: usize,
    },
    ComponentUtf16TooLong {
        actual: usize,
    },
    PathUtf8TooLong {
        actual: usize,
    },
    PathUtf16TooLong {
        actual: usize,
    },
    DuplicateMaterializedPath {
        path: String,
    },
    CaseFoldCollision {
        path: String,
    },
    DuplicateProjectionId {
        projection_id: String,
    },
    MissingParentDirectory {
        path: String,
    },
    InvalidFileMetadata,
    CountOverflow,
    InventoryDoesNotMatchOffer,
    ControlBindingMismatch,
    ReceiptBindingMismatch,
    DeclaredCountsMismatch,
    WritableMetadataMismatch,
    InvalidControlSequence,
    InvalidControlPath,
    UnsupportedArchiveEntryKind {
        kind: WorkspaceArchiveEntryKindV2,
    },
    MissingAuthorizedEntry,
    EntryKindMismatch,
    ArchiveDoesNotMatchInventory,
    ControlTooLarge {
        actual: usize,
    },
    InvalidJson(String),
    NonCanonicalControlJson,
    Api(crate::workspace_api_v2::WorkspaceApiV2ValidationError),
    Scope(crate::ScopeContractError),
}

impl Display for WorkspaceExportV2Error {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedApiGeneration { actual } => {
                write!(
                    formatter,
                    "workspace HTTP API generation {actual} is unsupported"
                )
            }
            Self::UnknownScopeOrdinal { actual } => {
                write!(
                    formatter,
                    "workspace export references unknown scope ordinal {actual}"
                )
            }
            Self::UnknownMount {
                scope_ordinal,
                mount_id,
            } => write!(
                formatter,
                "workspace export scope ordinal {scope_ordinal} references unknown mount `{mount_id}`"
            ),
            Self::InvalidScopeSourceAuthority => formatter.write_str(
                "workspace scope-to-source authority must cover every scope in canonical order and exactly match offered sources",
            ),
            Self::SourceNotInOffer {
                source_connection_id,
            } => write!(
                formatter,
                "workspace export source `{source_connection_id}` is absent from the sealed offer"
            ),
            Self::SourceScopeMismatch {
                scope_ordinal,
                expected,
                actual,
            } => write!(
                formatter,
                "workspace export scope ordinal {scope_ordinal} is bound to source `{expected}`, not `{actual}`"
            ),
            Self::UnknownTopLevelTarget { target } => {
                write!(
                    formatter,
                    "workspace export references unknown target `{target}`"
                )
            }
            Self::InvalidLogicalPath { path } => {
                write!(formatter, "workspace export path `{path}` is not portable")
            }
            Self::InvalidComponent { component } => {
                write!(
                    formatter,
                    "workspace export component `{component}` is invalid"
                )
            }
            Self::NonNfcComponent { component } => {
                write!(
                    formatter,
                    "workspace export component `{component}` is not Unicode NFC"
                )
            }
            Self::ComponentUtf8TooLong { actual } => write!(
                formatter,
                "workspace export component is {actual} UTF-8 bytes; maximum is {REQUIRED_MAX_COMPONENT_UTF8_BYTES}"
            ),
            Self::ComponentUtf16TooLong { actual } => write!(
                formatter,
                "workspace export component is {actual} UTF-16 units; maximum is {REQUIRED_MAX_COMPONENT_UTF16_UNITS}"
            ),
            Self::PathUtf8TooLong { actual } => write!(
                formatter,
                "workspace export path is {actual} UTF-8 bytes; maximum is {REQUIRED_MAX_PATH_UTF8_BYTES}"
            ),
            Self::PathUtf16TooLong { actual } => write!(
                formatter,
                "workspace export path is {actual} UTF-16 units; maximum is {REQUIRED_MAX_PATH_UTF16_UNITS}"
            ),
            Self::DuplicateMaterializedPath { path } => {
                write!(formatter, "workspace export path `{path}` is duplicated")
            }
            Self::CaseFoldCollision { path } => {
                write!(
                    formatter,
                    "workspace export path `{path}` has a Unicode case-fold collision"
                )
            }
            Self::DuplicateProjectionId { projection_id } => {
                write!(
                    formatter,
                    "workspace projection ID `{projection_id}` is duplicated"
                )
            }
            Self::MissingParentDirectory { path } => {
                write!(
                    formatter,
                    "workspace export path `{path}` is missing its parent directory"
                )
            }
            Self::InvalidFileMetadata => {
                formatter.write_str("workspace export file metadata is invalid")
            }
            Self::CountOverflow => formatter.write_str("workspace export counts exceed u64"),
            Self::InventoryDoesNotMatchOffer => {
                formatter.write_str("workspace inventory does not match the sealed offer")
            }
            Self::ControlBindingMismatch => formatter.write_str(
                "workspace control metadata does not match session, offer, layout, and inventory",
            ),
            Self::ReceiptBindingMismatch => formatter
                .write_str("workspace completion receipt does not match terminal control metadata"),
            Self::DeclaredCountsMismatch => {
                formatter.write_str("workspace target and archive counts do not match")
            }
            Self::WritableMetadataMismatch => formatter
                .write_str("workspace writable metadata does not match the namespaced inventory"),
            Self::InvalidControlSequence => formatter
                .write_str("workspace export control member must occur exactly once and last"),
            Self::InvalidControlPath => {
                formatter.write_str("workspace export control member has the wrong reserved path")
            }
            Self::UnsupportedArchiveEntryKind { kind } => write!(
                formatter,
                "workspace export archive entry kind {kind:?} is forbidden"
            ),
            Self::MissingAuthorizedEntry => {
                formatter.write_str("workspace export member is missing authorized metadata")
            }
            Self::EntryKindMismatch => formatter
                .write_str("workspace export member kind does not match authorized metadata"),
            Self::ArchiveDoesNotMatchInventory => formatter
                .write_str("workspace archive does not match its canonical namespaced inventory"),
            Self::ControlTooLarge { actual } => write!(
                formatter,
                "workspace terminal control is {actual} bytes; maximum is {MAX_EXPORT_TERMINAL_CONTROL_BYTES}"
            ),
            Self::InvalidJson(error) => write!(
                formatter,
                "invalid workspace terminal control JSON: {error}"
            ),
            Self::NonCanonicalControlJson => formatter.write_str(
                "workspace terminal control must use exact canonical compact JSON bytes",
            ),
            Self::Api(error) => Display::fmt(error, formatter),
            Self::Scope(error) => Display::fmt(error, formatter),
        }
    }
}

impl std::error::Error for WorkspaceExportV2Error {}

impl From<crate::workspace_api_v2::WorkspaceApiV2ValidationError> for WorkspaceExportV2Error {
    fn from(error: crate::workspace_api_v2::WorkspaceApiV2ValidationError) -> Self {
        Self::Api(error)
    }
}

impl From<crate::ScopeContractError> for WorkspaceExportV2Error {
    fn from(error: crate::ScopeContractError) -> Self {
        Self::Scope(error)
    }
}

fn plan_inventory(
    session_layout: &SessionLayout,
    offer: &WorkspaceExportOfferV2,
    scope_sources: &[WorkspaceScopeSourceAuthorityV2],
    entries: &[WorkspaceAuthorizedExportEntryV2],
) -> Result<WorkspaceNamespacedInventoryV2, WorkspaceExportV2Error> {
    let source_by_scope = validate_scope_source_authorities(session_layout, offer, scope_sources)?;
    let offered_sources = offer
        .offer()
        .source_generations
        .iter()
        .map(|generation| &generation.source_connection_id)
        .collect::<BTreeSet<_>>();
    let scope_map = session_layout
        .entries()
        .iter()
        .map(|entry| (entry.scope_ordinal(), (entry.mount_id(), entry.target())))
        .collect::<BTreeMap<_, _>>();
    let unique_targets = session_layout
        .entries()
        .iter()
        .map(|entry| (entry.mount_id().clone(), entry.target().clone()))
        .collect::<BTreeMap<_, _>>();

    let mut target_counts = unique_targets
        .iter()
        .map(|(mount_id, target)| {
            (
                mount_id.clone(),
                WorkspaceTargetDirectoryV2 {
                    mount_id: mount_id.clone(),
                    target: target.clone(),
                    directory_count: 1,
                    file_count: 0,
                    content_bytes: 0,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut directories = unique_targets
        .iter()
        .map(
            |(mount_id, target)| WorkspaceNamespacedExportRecordV2::TargetDirectory {
                mount_id: mount_id.clone(),
                target: target.clone(),
                member_path: target.as_str().to_string(),
            },
        )
        .collect::<Vec<_>>();
    let mut files = Vec::new();
    let mut paths = BTreeSet::new();
    let mut collision_keys = BTreeSet::new();
    let mut projection_ids = BTreeSet::new();

    for target in unique_targets.values() {
        insert_materialized_path(target.as_str(), &mut paths, &mut collision_keys)?;
    }

    for entry in entries {
        let scope_ordinal = entry.scope_ordinal();
        let Some((expected_mount_id, target)) = scope_map.get(&scope_ordinal) else {
            return Err(WorkspaceExportV2Error::UnknownScopeOrdinal {
                actual: scope_ordinal,
            });
        };
        if entry.mount_id() != *expected_mount_id {
            return Err(WorkspaceExportV2Error::UnknownMount {
                scope_ordinal,
                mount_id: entry.mount_id().as_str().to_string(),
            });
        }
        let logical_path = validate_connector_path(entry.logical_path())?;
        let member_path = format!("{}/{}", target.as_str(), logical_path.as_str());
        validate_regular_path(&member_path)?;
        insert_materialized_path(&member_path, &mut paths, &mut collision_keys)?;

        let counts = target_counts
            .get_mut(*expected_mount_id)
            .expect("session target count exists");
        match entry {
            WorkspaceAuthorizedExportEntryV2::Directory { .. } => {
                counts.directory_count = counts
                    .directory_count
                    .checked_add(1)
                    .ok_or(WorkspaceExportV2Error::CountOverflow)?;
                directories.push(WorkspaceNamespacedExportRecordV2::Directory {
                    winning_scope_ordinal: scope_ordinal,
                    mount_id: (*expected_mount_id).clone(),
                    target: (*target).clone(),
                    logical_path,
                    member_path,
                });
            }
            WorkspaceAuthorizedExportEntryV2::File {
                projection_id,
                source_connection_id,
                file_kind,
                effective_actions,
                content_sha256,
                byte_length,
                ..
            } => {
                if !offered_sources.contains(source_connection_id) {
                    return Err(WorkspaceExportV2Error::SourceNotInOffer {
                        source_connection_id: source_connection_id.as_str().to_string(),
                    });
                }
                let expected_source = source_by_scope
                    .get(&scope_ordinal)
                    .expect("every session scope has source authority");
                if source_connection_id != *expected_source {
                    return Err(WorkspaceExportV2Error::SourceScopeMismatch {
                        scope_ordinal,
                        expected: expected_source.as_str().to_string(),
                        actual: source_connection_id.as_str().to_string(),
                    });
                }
                validate_file_metadata(
                    projection_id,
                    source_connection_id,
                    file_kind,
                    effective_actions,
                    content_sha256,
                )?;
                if !projection_ids.insert(projection_id.as_str()) {
                    return Err(WorkspaceExportV2Error::DuplicateProjectionId {
                        projection_id: projection_id.as_str().to_string(),
                    });
                }
                counts.file_count = counts
                    .file_count
                    .checked_add(1)
                    .ok_or(WorkspaceExportV2Error::CountOverflow)?;
                counts.content_bytes = counts
                    .content_bytes
                    .checked_add(*byte_length)
                    .ok_or(WorkspaceExportV2Error::CountOverflow)?;
                files.push(WorkspaceNamespacedExportRecordV2::File {
                    winning_scope_ordinal: scope_ordinal,
                    mount_id: (*expected_mount_id).clone(),
                    target: (*target).clone(),
                    logical_path,
                    member_path,
                    projection_id: projection_id.clone(),
                    source_connection_id: source_connection_id.clone(),
                    file_kind: file_kind.clone(),
                    effective_actions: effective_actions.clone(),
                    content_sha256: content_sha256.clone(),
                    byte_length: *byte_length,
                });
            }
        }
    }

    directories.sort_by(|left, right| {
        path_depth(left.member_path())
            .cmp(&path_depth(right.member_path()))
            .then_with(|| {
                left.member_path()
                    .as_bytes()
                    .cmp(right.member_path().as_bytes())
            })
    });
    files.sort_by(file_record_order);
    validate_parent_directories(&directories, &files)?;

    let mut records = directories;
    records.extend(files);
    records.push(WorkspaceNamespacedExportRecordV2::Control {
        member_path: RESERVED_EXPORT_METADATA_PATH.to_string(),
    });
    let target_directories = target_counts.into_values().collect::<Vec<_>>();
    let (directory_count, file_count, selected_content_bytes) =
        sum_target_counts(&target_directories)?;
    let archive_entry_count = directory_count
        .checked_add(file_count)
        .and_then(|count| count.checked_add(1))
        .ok_or(WorkspaceExportV2Error::CountOverflow)?;
    let inventory_sha256 = format!(
        "sha256:{:x}",
        Sha256::digest(canonical_inventory_preimage(scope_sources, &records)?)
    );
    Ok(WorkspaceNamespacedInventoryV2 {
        scope_sources: scope_sources.to_vec(),
        target_directories,
        records,
        control_entry_count: 1,
        file_count,
        directory_count,
        archive_entry_count,
        selected_content_bytes,
        inventory_sha256,
    })
}

fn canonical_inventory_preimage(
    scope_sources: &[WorkspaceScopeSourceAuthorityV2],
    records: &[WorkspaceNamespacedExportRecordV2],
) -> Result<Vec<u8>, WorkspaceExportV2Error> {
    let mut output = WORKSPACE_EXPORT_INVENTORY_V2_DOMAIN.to_vec();
    append_count(&mut output, scope_sources.len())?;
    for authority in scope_sources {
        append_u64(&mut output, u64::from(authority.scope_ordinal))?;
        append_text(&mut output, authority.source_connection_id.as_str())?;
    }
    let inventory_count = records
        .len()
        .checked_sub(1)
        .ok_or(WorkspaceExportV2Error::InvalidControlSequence)?;
    append_count(&mut output, inventory_count)?;
    for record in records {
        match record {
            WorkspaceNamespacedExportRecordV2::TargetDirectory {
                mount_id,
                target,
                member_path,
            } => {
                append_text(&mut output, "target_directory")?;
                append_text(&mut output, mount_id.as_str())?;
                append_text(&mut output, target.as_str())?;
                append_text(&mut output, member_path)?;
            }
            WorkspaceNamespacedExportRecordV2::Directory {
                winning_scope_ordinal,
                mount_id,
                target,
                logical_path,
                member_path,
            } => {
                append_text(&mut output, "directory")?;
                append_u64(&mut output, u64::from(*winning_scope_ordinal))?;
                append_text(&mut output, mount_id.as_str())?;
                append_text(&mut output, target.as_str())?;
                append_text(&mut output, logical_path.as_str())?;
                append_text(&mut output, member_path)?;
            }
            WorkspaceNamespacedExportRecordV2::File {
                winning_scope_ordinal,
                mount_id,
                target,
                logical_path,
                member_path,
                projection_id,
                source_connection_id,
                file_kind,
                effective_actions,
                content_sha256,
                byte_length,
            } => {
                append_text(&mut output, "file")?;
                append_u64(&mut output, u64::from(*winning_scope_ordinal))?;
                append_text(&mut output, mount_id.as_str())?;
                append_text(&mut output, target.as_str())?;
                append_text(&mut output, source_connection_id.as_str())?;
                append_text(&mut output, projection_id.as_str())?;
                append_text(&mut output, logical_path.as_str())?;
                append_text(&mut output, member_path)?;
                append_text(&mut output, projection_file_kind_wire_label(file_kind))?;
                append_count(&mut output, effective_actions.len())?;
                let mut actions = effective_actions
                    .iter()
                    .map(source_action_wire_label)
                    .collect::<Vec<_>>();
                actions.sort_unstable();
                for action in actions {
                    append_text(&mut output, action)?;
                }
                append_text(&mut output, content_sha256)?;
                append_u64(&mut output, *byte_length)?;
            }
            WorkspaceNamespacedExportRecordV2::Control { .. } => {}
        }
    }
    Ok(output)
}

fn append_count(output: &mut Vec<u8>, count: usize) -> Result<(), WorkspaceExportV2Error> {
    append_u64(
        output,
        u64::try_from(count).map_err(|_| WorkspaceExportV2Error::CountOverflow)?,
    )
}

fn append_text(output: &mut Vec<u8>, value: &str) -> Result<(), WorkspaceExportV2Error> {
    append_count(output, value.len())?;
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn append_u64(output: &mut Vec<u8>, value: u64) -> Result<(), WorkspaceExportV2Error> {
    output.extend_from_slice(&value.to_be_bytes());
    Ok(())
}

fn validate_connector_path(value: &str) -> Result<LogicalPath, WorkspaceExportV2Error> {
    let logical_path =
        LogicalPath::new(value).map_err(|_| WorkspaceExportV2Error::InvalidLogicalPath {
            path: value.to_string(),
        })?;
    validate_regular_path(value)?;
    Ok(logical_path)
}

fn validate_regular_path(value: &str) -> Result<(), WorkspaceExportV2Error> {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value.split('/').any(|component| component.is_empty())
    {
        return Err(WorkspaceExportV2Error::InvalidLogicalPath {
            path: value.to_string(),
        });
    }
    for component in value.split('/') {
        validate_component(component)?;
    }
    if value.len() > usize::from(REQUIRED_MAX_PATH_UTF8_BYTES) {
        return Err(WorkspaceExportV2Error::PathUtf8TooLong {
            actual: value.len(),
        });
    }
    let utf16_units = value.encode_utf16().count();
    if utf16_units > usize::from(REQUIRED_MAX_PATH_UTF16_UNITS) {
        return Err(WorkspaceExportV2Error::PathUtf16TooLong {
            actual: utf16_units,
        });
    }
    Ok(())
}

fn validate_component(component: &str) -> Result<(), WorkspaceExportV2Error> {
    if matches!(component, "." | "..")
        || component.chars().any(char::is_control)
        || component.contains([':', '<', '>', '"', '|', '?', '*'])
        || component.ends_with(['.', ' '])
        || is_windows_device_name(component)
    {
        return Err(WorkspaceExportV2Error::InvalidComponent {
            component: component.to_string(),
        });
    }
    if !component.nfc().eq(component.chars()) {
        return Err(WorkspaceExportV2Error::NonNfcComponent {
            component: component.to_string(),
        });
    }
    if component.len() > usize::from(REQUIRED_MAX_COMPONENT_UTF8_BYTES) {
        return Err(WorkspaceExportV2Error::ComponentUtf8TooLong {
            actual: component.len(),
        });
    }
    let utf16_units = component.encode_utf16().count();
    if utf16_units > usize::from(REQUIRED_MAX_COMPONENT_UTF16_UNITS) {
        return Err(WorkspaceExportV2Error::ComponentUtf16TooLong {
            actual: utf16_units,
        });
    }
    Ok(())
}

fn is_windows_device_name(value: &str) -> bool {
    let stem = value.split('.').next().unwrap_or(value);
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|suffix| matches!(suffix.as_bytes(), [b'1'..=b'9']))
}

fn collision_key(value: &str) -> String {
    value.chars().default_case_fold().nfc().collect()
}

fn insert_materialized_path(
    path: &str,
    paths: &mut BTreeSet<String>,
    collision_keys: &mut BTreeSet<String>,
) -> Result<(), WorkspaceExportV2Error> {
    if !paths.insert(path.to_string()) {
        return Err(WorkspaceExportV2Error::DuplicateMaterializedPath {
            path: path.to_string(),
        });
    }
    if !collision_keys.insert(collision_key(path)) {
        return Err(WorkspaceExportV2Error::CaseFoldCollision {
            path: path.to_string(),
        });
    }
    Ok(())
}

fn validate_file_metadata(
    projection_id: &ProjectionId,
    source_connection_id: &SourceConnectionId,
    file_kind: &ProjectionFileKind,
    effective_actions: &BTreeSet<SourceAction>,
    content_sha256: &str,
) -> Result<(), WorkspaceExportV2Error> {
    if projection_id.as_str().is_empty()
        || source_connection_id.as_str().is_empty()
        || *file_kind == ProjectionFileKind::Directory
        || effective_actions.is_empty()
        || validate_sha256(content_sha256).is_err()
    {
        return Err(WorkspaceExportV2Error::InvalidFileMetadata);
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), WorkspaceExportV2Error> {
    if value.len() != 71
        || !value.starts_with("sha256:")
        || !value.as_bytes()[7..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
    {
        return Err(WorkspaceExportV2Error::InvalidFileMetadata);
    }
    Ok(())
}

fn file_record_order(
    left: &WorkspaceNamespacedExportRecordV2,
    right: &WorkspaceNamespacedExportRecordV2,
) -> std::cmp::Ordering {
    file_record_key(left).cmp(&file_record_key(right))
}

fn file_record_key(record: &WorkspaceNamespacedExportRecordV2) -> (u32, Option<&str>, &str, &str) {
    match record {
        WorkspaceNamespacedExportRecordV2::File {
            winning_scope_ordinal,
            logical_path,
            projection_id,
            ..
        } => (
            *winning_scope_ordinal,
            logical_path
                .as_str()
                .rsplit_once('/')
                .map(|(parent, _)| parent),
            logical_path.as_str(),
            projection_id.as_str(),
        ),
        _ => unreachable!("only file records are sorted here"),
    }
}

fn path_depth(path: &str) -> usize {
    path.split('/').count()
}

fn validate_parent_directories(
    directories: &[WorkspaceNamespacedExportRecordV2],
    files: &[WorkspaceNamespacedExportRecordV2],
) -> Result<(), WorkspaceExportV2Error> {
    let directory_paths = directories
        .iter()
        .map(WorkspaceNamespacedExportRecordV2::member_path)
        .collect::<BTreeSet<_>>();
    for record in directories.iter().chain(files) {
        if let Some((parent, _)) = record.member_path().rsplit_once('/')
            && !directory_paths.contains(parent)
        {
            return Err(WorkspaceExportV2Error::MissingParentDirectory {
                path: record.member_path().to_string(),
            });
        }
    }
    Ok(())
}

fn sum_target_counts(
    targets: &[WorkspaceTargetDirectoryV2],
) -> Result<(u64, u64, u64), WorkspaceExportV2Error> {
    targets
        .iter()
        .try_fold((0_u64, 0_u64, 0_u64), |totals, target| {
            Ok((
                totals
                    .0
                    .checked_add(target.directory_count)
                    .ok_or(WorkspaceExportV2Error::CountOverflow)?,
                totals
                    .1
                    .checked_add(target.file_count)
                    .ok_or(WorkspaceExportV2Error::CountOverflow)?,
                totals
                    .2
                    .checked_add(target.content_bytes)
                    .ok_or(WorkspaceExportV2Error::CountOverflow)?,
            ))
        })
}

fn validate_target_declarations(
    targets: &[WorkspaceTargetDirectoryV2],
) -> Result<(), WorkspaceExportV2Error> {
    if targets.is_empty() {
        return Err(WorkspaceExportV2Error::DeclaredCountsMismatch);
    }
    let mut previous_mount_id: Option<&PortableMountId> = None;
    let mut target_keys = BTreeSet::new();
    for target in targets {
        if target.directory_count == 0
            || previous_mount_id.is_some_and(|previous| previous >= &target.mount_id)
            || !target_keys.insert(target.target.collision_key())
        {
            return Err(WorkspaceExportV2Error::DeclaredCountsMismatch);
        }
        previous_mount_id = Some(&target.mount_id);
    }
    Ok(())
}

fn validate_scope_source_shape(
    authorities: &[WorkspaceScopeSourceAuthorityV2],
) -> Result<(), WorkspaceExportV2Error> {
    if authorities.is_empty() {
        return Err(WorkspaceExportV2Error::InvalidScopeSourceAuthority);
    }
    for (index, authority) in authorities.iter().enumerate() {
        let expected = u32::try_from(index).map_err(|_| WorkspaceExportV2Error::CountOverflow)?;
        if authority.scope_ordinal != expected || authority.source_connection_id.as_str().is_empty()
        {
            return Err(WorkspaceExportV2Error::InvalidScopeSourceAuthority);
        }
    }
    Ok(())
}

fn validate_scope_source_authorities<'a>(
    session_layout: &SessionLayout,
    offer: &'a WorkspaceExportOfferV2,
    authorities: &'a [WorkspaceScopeSourceAuthorityV2],
) -> Result<BTreeMap<u32, &'a SourceConnectionId>, WorkspaceExportV2Error> {
    offer.validate()?;
    if session_layout.layout_version() != offer.layout_version()
        || session_layout.layout_digest() != offer.layout_digest()
        || authorities.len() != session_layout.entries().len()
    {
        return Err(WorkspaceExportV2Error::InvalidScopeSourceAuthority);
    }
    validate_scope_source_shape(authorities)?;

    let offered_sources = offer
        .offer()
        .source_generations
        .iter()
        .map(|generation| &generation.source_connection_id)
        .collect::<BTreeSet<_>>();
    let authority_sources = authorities
        .iter()
        .map(|authority| &authority.source_connection_id)
        .collect::<BTreeSet<_>>();
    if offered_sources != authority_sources {
        return Err(WorkspaceExportV2Error::InvalidScopeSourceAuthority);
    }

    Ok(authorities
        .iter()
        .map(|authority| (authority.scope_ordinal, &authority.source_connection_id))
        .collect())
}

fn validate_writable_entries(
    metadata: &ScopeAuthorizedWritableExportMetadata,
    inventory: &WorkspaceNamespacedInventoryV2,
) -> Result<(), WorkspaceExportV2Error> {
    let files = inventory
        .records
        .iter()
        .filter_map(|record| match record {
            WorkspaceNamespacedExportRecordV2::File {
                projection_id,
                logical_path,
                effective_actions,
                content_sha256,
                ..
            } => Some((
                projection_id,
                (logical_path, effective_actions, content_sha256),
            )),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    for entry in &metadata.writable_entries {
        let Some((logical_path, effective_actions, content_sha256)) =
            files.get(&entry.projection_id)
        else {
            return Err(WorkspaceExportV2Error::WritableMetadataMismatch);
        };
        if *logical_path != &entry.logical_path
            || *effective_actions != &entry.effective_actions
            || *content_sha256 != &entry.delivered_content_sha256
        {
            return Err(WorkspaceExportV2Error::WritableMetadataMismatch);
        }
    }
    Ok(())
}

fn validate_control_sequence(
    members: &[WorkspaceArchiveMemberV2],
) -> Result<(), WorkspaceExportV2Error> {
    let Some(last) = members.last() else {
        return Err(WorkspaceExportV2Error::InvalidControlSequence);
    };
    if last.kind != WorkspaceArchiveEntryKindV2::Control
        || members[..members.len() - 1]
            .iter()
            .any(|member| member.kind == WorkspaceArchiveEntryKindV2::Control)
    {
        return Err(WorkspaceExportV2Error::InvalidControlSequence);
    }
    if last.member_path != RESERVED_EXPORT_METADATA_PATH {
        return Err(WorkspaceExportV2Error::InvalidControlPath);
    }
    Ok(())
}

fn validate_stream_member_path(
    member: &WorkspaceArchiveMemberV2,
    targets: &BTreeSet<&str>,
) -> Result<(), WorkspaceExportV2Error> {
    if member.kind == WorkspaceArchiveEntryKindV2::Control {
        return (member.member_path == RESERVED_EXPORT_METADATA_PATH)
            .then_some(())
            .ok_or(WorkspaceExportV2Error::InvalidControlPath);
    }
    validate_regular_path(&member.member_path)?;
    let top_level = member.member_path.split('/').next().unwrap_or_default();
    if !targets.contains(top_level) {
        return Err(WorkspaceExportV2Error::UnknownTopLevelTarget {
            target: top_level.to_string(),
        });
    }
    Ok(())
}

fn materialization_entry(
    record: &WorkspaceNamespacedExportRecordV2,
) -> Option<WorkspaceMaterializationEntryV2> {
    match record {
        WorkspaceNamespacedExportRecordV2::TargetDirectory {
            mount_id,
            target,
            member_path,
        } => Some(WorkspaceMaterializationEntryV2 {
            kind: WorkspaceArchiveEntryKindV2::Directory,
            mount_id: mount_id.clone(),
            target: target.clone(),
            logical_path: None,
            member_path: member_path.clone(),
            projection_id: None,
        }),
        WorkspaceNamespacedExportRecordV2::Directory {
            mount_id,
            target,
            logical_path,
            member_path,
            ..
        } => Some(WorkspaceMaterializationEntryV2 {
            kind: WorkspaceArchiveEntryKindV2::Directory,
            mount_id: mount_id.clone(),
            target: target.clone(),
            logical_path: Some(logical_path.clone()),
            member_path: member_path.clone(),
            projection_id: None,
        }),
        WorkspaceNamespacedExportRecordV2::File {
            mount_id,
            target,
            logical_path,
            member_path,
            projection_id,
            ..
        } => Some(WorkspaceMaterializationEntryV2 {
            kind: WorkspaceArchiveEntryKindV2::File,
            mount_id: mount_id.clone(),
            target: target.clone(),
            logical_path: Some(logical_path.clone()),
            member_path: member_path.clone(),
            projection_id: Some(projection_id.clone()),
        }),
        WorkspaceNamespacedExportRecordV2::Control { .. } => None,
    }
}

fn authorized_entry_from_record(
    record: &WorkspaceNamespacedExportRecordV2,
) -> Option<WorkspaceAuthorizedExportEntryV2> {
    match record {
        WorkspaceNamespacedExportRecordV2::Directory {
            winning_scope_ordinal,
            mount_id,
            logical_path,
            ..
        } => Some(WorkspaceAuthorizedExportEntryV2::Directory {
            winning_scope_ordinal: *winning_scope_ordinal,
            mount_id: mount_id.clone(),
            logical_path: logical_path.as_str().to_string(),
        }),
        WorkspaceNamespacedExportRecordV2::File {
            winning_scope_ordinal,
            mount_id,
            logical_path,
            projection_id,
            source_connection_id,
            file_kind,
            effective_actions,
            content_sha256,
            byte_length,
            ..
        } => Some(WorkspaceAuthorizedExportEntryV2::File {
            winning_scope_ordinal: *winning_scope_ordinal,
            mount_id: mount_id.clone(),
            logical_path: logical_path.as_str().to_string(),
            projection_id: projection_id.clone(),
            source_connection_id: source_connection_id.clone(),
            file_kind: file_kind.clone(),
            effective_actions: effective_actions.clone(),
            content_sha256: content_sha256.clone(),
            byte_length: *byte_length,
        }),
        WorkspaceNamespacedExportRecordV2::TargetDirectory { .. }
        | WorkspaceNamespacedExportRecordV2::Control { .. } => None,
    }
}
