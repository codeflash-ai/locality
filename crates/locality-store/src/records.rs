//! Durable record shapes.
//!
//! These types are deliberately close to `locality-core` value types. The store owns
//! persistence identity and lookup concerns, while the core owns sync semantics.

use std::path::PathBuf;

use locality_core::freshness::{FreshnessTier, RemoteObservation, RemoteVersion};
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId, SourceSpan, TreeEntry};
use locality_core::shadow::{MarkdownBlockKind, ShadowBlock, ShadowDocument};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ConnectionId(pub String);

impl ConnectionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ConnectorProfileId(pub String);

impl ConnectorProfileId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionMode {
    PlainFiles,
    MacosFileProvider,
    LinuxFuse,
    WindowsCloudFiles,
}

impl ProjectionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PlainFiles => "plain_files",
            Self::MacosFileProvider => "macos_file_provider",
            Self::LinuxFuse => "linux_fuse",
            Self::WindowsCloudFiles => "windows_cloud_files",
        }
    }

    pub fn uses_virtual_filesystem(&self) -> bool {
        matches!(
            self,
            Self::MacosFileProvider | Self::LinuxFuse | Self::WindowsCloudFiles
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountConfig {
    pub mount_id: MountId,
    pub connector: String,
    pub root: PathBuf,
    pub remote_root_id: Option<RemoteId>,
    pub connection_id: Option<ConnectionId>,
    pub read_only: bool,
    pub projection: ProjectionMode,
}

impl MountConfig {
    pub fn new(mount_id: MountId, connector: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            mount_id,
            connector: connector.into(),
            root: root.into(),
            remote_root_id: None,
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
        }
    }

    pub fn with_remote_root_id(mut self, remote_root_id: RemoteId) -> Self {
        self.remote_root_id = Some(remote_root_id);
        self
    }

    pub fn with_connection_id(mut self, connection_id: ConnectionId) -> Self {
        self.connection_id = Some(connection_id);
        self
    }

    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    pub fn projection(mut self, projection: ProjectionMode) -> Self {
        self.projection = projection;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountLiveModeState {
    Off,
    Active,
    Syncing,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountLiveModeRecord {
    pub mount_id: MountId,
    pub enabled: bool,
    pub state: MountLiveModeState,
    pub last_reason: Option<String>,
    pub last_run_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl MountLiveModeRecord {
    pub fn new(mount_id: MountId, enabled: bool, created_at: impl Into<String>) -> Self {
        let created_at = created_at.into();
        Self {
            mount_id,
            enabled,
            state: if enabled {
                MountLiveModeState::Active
            } else {
                MountLiveModeState::Off
            },
            last_reason: None,
            last_run_at: None,
            created_at: created_at.clone(),
            updated_at: created_at,
        }
    }

    pub fn off(mut self, updated_at: impl Into<String>) -> Self {
        self.enabled = false;
        self.state = MountLiveModeState::Off;
        self.last_reason = None;
        self.updated_at = updated_at.into();
        self
    }

    pub fn active(
        mut self,
        reason: Option<String>,
        last_run_at: impl Into<String>,
        updated_at: impl Into<String>,
    ) -> Self {
        self.enabled = true;
        self.state = MountLiveModeState::Active;
        self.last_reason = reason;
        self.last_run_at = Some(last_run_at.into());
        self.updated_at = updated_at.into();
        self
    }

    pub fn syncing(mut self, updated_at: impl Into<String>) -> Self {
        self.enabled = true;
        self.state = MountLiveModeState::Syncing;
        self.last_reason = None;
        self.updated_at = updated_at.into();
        self
    }

    pub fn error(
        mut self,
        reason: impl Into<String>,
        last_run_at: impl Into<String>,
        updated_at: impl Into<String>,
    ) -> Self {
        self.enabled = false;
        self.state = MountLiveModeState::Error;
        self.last_reason = Some(reason.into());
        self.last_run_at = Some(last_run_at.into());
        self.updated_at = updated_at.into();
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionRecord {
    pub connection_id: ConnectionId,
    pub profile_id: Option<ConnectorProfileId>,
    pub connector: String,
    pub display_name: String,
    pub account_label: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
    pub auth_kind: String,
    pub secret_ref: String,
    pub scopes: Vec<String>,
    pub capabilities_json: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub expires_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorProfileRecord {
    pub profile_id: ConnectorProfileId,
    pub connector: String,
    pub display_name: String,
    pub auth_kind: String,
    pub scopes: Vec<String>,
    pub capabilities_json: String,
    pub enabled_actions_json: String,
    pub connector_version: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRecord {
    pub mount_id: MountId,
    pub remote_id: RemoteId,
    pub kind: EntityKind,
    pub title: String,
    pub path: PathBuf,
    pub hydration: HydrationState,
    pub content_hash: Option<String>,
    /// Remote version represented by the Synced Tree shadow.
    ///
    /// The field name stays `remote_edited_at` for schema/frontmatter
    /// compatibility with existing Notion mounts. New sync code should use the
    /// `synced_tree_remote_version` helpers so this is not confused with the
    /// latest Remote Tree observation.
    pub remote_edited_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualMutationKind {
    Create,
    Move,
    Rename,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualMutationRecord {
    pub mount_id: MountId,
    pub local_id: String,
    pub mutation_kind: VirtualMutationKind,
    pub target_remote_id: Option<RemoteId>,
    pub parent_remote_id: Option<RemoteId>,
    pub original_path: Option<PathBuf>,
    pub projected_path: PathBuf,
    pub title: String,
    pub content_path: Option<PathBuf>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoSaveOrigin {
    LocalityCreated,
    UserEnabled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoSaveState {
    Active,
    Blocked,
    PausedRemoteChanged,
    PausedFailure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoSaveEnrollmentRecord {
    pub mount_id: MountId,
    pub path: PathBuf,
    pub remote_id: Option<RemoteId>,
    pub enabled: bool,
    pub origin: AutoSaveOrigin,
    pub state: AutoSaveState,
    pub last_reason: Option<String>,
    pub last_push_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl AutoSaveEnrollmentRecord {
    pub fn new(
        mount_id: MountId,
        path: impl Into<PathBuf>,
        origin: AutoSaveOrigin,
        created_at: impl Into<String>,
    ) -> Self {
        let created_at = created_at.into();
        Self {
            mount_id,
            path: path.into(),
            remote_id: None,
            enabled: true,
            origin,
            state: AutoSaveState::Active,
            last_reason: None,
            last_push_id: None,
            created_at: created_at.clone(),
            updated_at: created_at,
        }
    }

    pub fn disabled(mut self, updated_at: impl Into<String>) -> Self {
        self.enabled = false;
        self.state = AutoSaveState::Active;
        self.last_reason = None;
        self.updated_at = updated_at.into();
        self
    }

    pub fn active(mut self, updated_at: impl Into<String>) -> Self {
        self.enabled = true;
        self.state = AutoSaveState::Active;
        self.last_reason = None;
        self.updated_at = updated_at.into();
        self
    }

    pub fn blocked(mut self, reason: impl Into<String>, updated_at: impl Into<String>) -> Self {
        self.enabled = true;
        self.state = AutoSaveState::Blocked;
        self.last_reason = Some(reason.into());
        self.updated_at = updated_at.into();
        self
    }

    pub fn paused_remote_changed(
        mut self,
        reason: impl Into<String>,
        updated_at: impl Into<String>,
    ) -> Self {
        self.enabled = true;
        self.state = AutoSaveState::PausedRemoteChanged;
        self.last_reason = Some(reason.into());
        self.updated_at = updated_at.into();
        self
    }

    pub fn paused_failure(
        mut self,
        reason: impl Into<String>,
        updated_at: impl Into<String>,
    ) -> Self {
        self.enabled = true;
        self.state = AutoSaveState::PausedFailure;
        self.last_reason = Some(reason.into());
        self.updated_at = updated_at.into();
        self
    }
}

/// Latest known metadata for the Nucleus Remote Tree.
///
/// This is intentionally separate from `EntityRecord::remote_edited_at`, which
/// belongs to the Synced Tree. Observation updates must not advance shadows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteObservationRecord {
    pub mount_id: MountId,
    pub remote_id: RemoteId,
    pub kind: EntityKind,
    pub title: String,
    pub parent_remote_id: Option<RemoteId>,
    pub projected_path: PathBuf,
    pub remote_version: Option<RemoteVersion>,
    pub observed_at: String,
    pub deleted: bool,
    pub raw_metadata_json: String,
}

impl RemoteObservationRecord {
    pub fn new(
        mount_id: MountId,
        remote_id: RemoteId,
        kind: EntityKind,
        title: impl Into<String>,
        projected_path: impl Into<PathBuf>,
        observed_at: impl Into<String>,
    ) -> Self {
        Self {
            mount_id,
            remote_id,
            kind,
            title: title.into(),
            parent_remote_id: None,
            projected_path: projected_path.into(),
            remote_version: None,
            observed_at: observed_at.into(),
            deleted: false,
            raw_metadata_json: "{}".to_string(),
        }
    }

    pub fn with_parent(mut self, parent_remote_id: RemoteId) -> Self {
        self.parent_remote_id = Some(parent_remote_id);
        self
    }

    pub fn with_remote_version(mut self, remote_version: RemoteVersion) -> Self {
        self.remote_version = Some(remote_version);
        self
    }

    pub fn deleted(mut self, deleted: bool) -> Self {
        self.deleted = deleted;
        self
    }

    pub fn with_raw_metadata_json(mut self, raw_metadata_json: impl Into<String>) -> Self {
        self.raw_metadata_json = raw_metadata_json.into();
        self
    }
}

impl From<RemoteObservationRecord> for RemoteObservation {
    fn from(value: RemoteObservationRecord) -> Self {
        Self {
            mount_id: value.mount_id,
            remote_id: value.remote_id,
            kind: value.kind,
            title: value.title,
            parent_remote_id: value.parent_remote_id,
            projected_path: value.projected_path,
            remote_version: value.remote_version,
            deleted: value.deleted,
            raw_metadata_json: value.raw_metadata_json,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessStateRecord {
    pub mount_id: MountId,
    pub remote_id: RemoteId,
    pub tier: FreshnessTier,
    pub last_checked_at: Option<String>,
    pub next_check_at: Option<String>,
    pub last_opened_at: Option<String>,
    pub last_local_change_at: Option<String>,
    pub remote_hint_pending: bool,
}

impl FreshnessStateRecord {
    pub fn new(mount_id: MountId, remote_id: RemoteId, tier: FreshnessTier) -> Self {
        Self {
            mount_id,
            remote_id,
            tier,
            last_checked_at: None,
            next_check_at: None,
            last_opened_at: None,
            last_local_change_at: None,
            remote_hint_pending: false,
        }
    }

    pub fn checked_at(mut self, last_checked_at: impl Into<String>) -> Self {
        self.last_checked_at = Some(last_checked_at.into());
        self
    }

    pub fn next_check_at(mut self, next_check_at: impl Into<String>) -> Self {
        self.next_check_at = Some(next_check_at.into());
        self
    }

    pub fn opened_at(mut self, last_opened_at: impl Into<String>) -> Self {
        self.last_opened_at = Some(last_opened_at.into());
        self
    }

    pub fn local_change_at(mut self, last_local_change_at: impl Into<String>) -> Self {
        self.last_local_change_at = Some(last_local_change_at.into());
        self
    }

    pub fn remote_hint_pending(mut self, remote_hint_pending: bool) -> Self {
        self.remote_hint_pending = remote_hint_pending;
        self
    }
}

impl EntityRecord {
    pub fn new(
        mount_id: MountId,
        remote_id: RemoteId,
        kind: EntityKind,
        title: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            mount_id,
            remote_id,
            kind,
            title: title.into(),
            path: path.into(),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: None,
        }
    }

    pub fn with_hydration(mut self, hydration: HydrationState) -> Self {
        self.hydration = hydration;
        self
    }

    pub fn with_content_hash(mut self, content_hash: impl Into<String>) -> Self {
        self.content_hash = Some(content_hash.into());
        self
    }

    pub fn synced_tree_remote_version(&self) -> Option<&str> {
        self.remote_edited_at.as_deref()
    }

    pub fn set_synced_tree_remote_version(&mut self, version: Option<String>) {
        self.remote_edited_at = version;
    }

    pub fn with_synced_tree_remote_version(mut self, version: impl Into<String>) -> Self {
        self.set_synced_tree_remote_version(Some(version.into()));
        self
    }

    pub fn with_remote_edited_at(self, remote_edited_at: impl Into<String>) -> Self {
        self.with_synced_tree_remote_version(remote_edited_at)
    }
}

impl From<TreeEntry> for EntityRecord {
    fn from(value: TreeEntry) -> Self {
        Self {
            mount_id: value.mount_id,
            remote_id: value.remote_id,
            kind: value.kind,
            title: value.title,
            path: value.path,
            hydration: value.hydration,
            content_hash: value.content_hash,
            remote_edited_at: value.remote_edited_at,
        }
    }
}

impl From<EntityRecord> for TreeEntry {
    fn from(value: EntityRecord) -> Self {
        Self {
            mount_id: value.mount_id,
            remote_id: value.remote_id,
            kind: value.kind,
            title: value.title,
            path: value.path,
            hydration: value.hydration,
            content_hash: value.content_hash,
            remote_edited_at: value.remote_edited_at,
            stub_frontmatter: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HydrationJobRecord {
    pub mount_id: MountId,
    pub remote_id: RemoteId,
    pub path: PathBuf,
    pub target_state: HydrationState,
    pub reason: HydrationReason,
    pub attempts: u32,
    pub last_error: Option<String>,
}

impl HydrationJobRecord {
    pub fn new(request: HydrationRequest) -> Self {
        Self {
            mount_id: request.mount_id,
            remote_id: request.remote_id,
            path: request.path,
            target_state: request.target_state,
            reason: request.reason,
            attempts: 0,
            last_error: None,
        }
    }

    pub fn into_request(self) -> HydrationRequest {
        HydrationRequest {
            mount_id: self.mount_id,
            remote_id: self.remote_id,
            path: self.path,
            target_state: self.target_state,
            reason: self.reason,
        }
    }
}

impl From<HydrationRequest> for HydrationJobRecord {
    fn from(value: HydrationRequest) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataDiscoveryPriority {
    Background,
    Interactive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataDiscoveryJobRecord {
    pub mount_id: MountId,
    pub container_identifier: String,
    pub priority: MetadataDiscoveryPriority,
    pub depth: u32,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowSnapshotRecord {
    pub mount_id: MountId,
    pub entity_id: RemoteId,
    #[serde(default)]
    pub frontmatter: String,
    pub body_hash: String,
    pub rendered_body: String,
    pub blocks: Vec<ShadowBlockRecord>,
}

impl ShadowSnapshotRecord {
    pub fn from_document(mount_id: MountId, document: &ShadowDocument) -> Self {
        Self {
            mount_id,
            entity_id: document.entity_id.clone(),
            frontmatter: document.frontmatter.clone(),
            body_hash: document.body_hash.clone(),
            rendered_body: document.rendered_body.clone(),
            blocks: document
                .blocks
                .iter()
                .cloned()
                .map(ShadowBlockRecord::from)
                .collect(),
        }
    }

    pub fn into_document(self) -> ShadowDocument {
        ShadowDocument {
            entity_id: self.entity_id,
            frontmatter: self.frontmatter,
            body_hash: self.body_hash,
            rendered_body: self.rendered_body,
            blocks: self.blocks.into_iter().map(ShadowBlock::from).collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowBlockRecord {
    pub remote_id: RemoteId,
    pub kind: MarkdownBlockKind,
    pub source_span: SourceSpan,
    pub content_hash: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_kind: Option<String>,
}

impl From<ShadowBlock> for ShadowBlockRecord {
    fn from(value: ShadowBlock) -> Self {
        Self {
            remote_id: value.remote_id,
            kind: value.kind,
            source_span: value.source_span,
            content_hash: value.content_hash,
            text: value.text,
            native_kind: value.native_kind,
        }
    }
}

impl From<ShadowBlockRecord> for ShadowBlock {
    fn from(value: ShadowBlockRecord) -> Self {
        Self {
            remote_id: value.remote_id,
            kind: value.kind,
            source_span: value.source_span,
            content_hash: value.content_hash,
            text: value.text,
            native_kind: value.native_kind,
        }
    }
}
