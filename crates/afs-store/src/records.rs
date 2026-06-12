//! Durable record shapes.
//!
//! These types are deliberately close to `afs-core` value types. The store owns
//! persistence identity and lookup concerns, while the core owns sync semantics.

use std::path::PathBuf;

use afs_core::hydration::{HydrationReason, HydrationRequest};
use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId, SourceSpan, TreeEntry};
use afs_core::shadow::{MarkdownBlockKind, ShadowBlock, ShadowDocument};
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
}

impl ProjectionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PlainFiles => "plain_files",
            Self::MacosFileProvider => "macos_file_provider",
            Self::LinuxFuse => "linux_fuse",
        }
    }

    pub fn uses_virtual_filesystem(&self) -> bool {
        matches!(self, Self::MacosFileProvider | Self::LinuxFuse)
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
    pub remote_edited_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualMutationKind {
    Create,
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

    pub fn with_remote_edited_at(mut self, remote_edited_at: impl Into<String>) -> Self {
        self.remote_edited_at = Some(remote_edited_at.into());
        self
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
}

impl From<ShadowBlock> for ShadowBlockRecord {
    fn from(value: ShadowBlock) -> Self {
        Self {
            remote_id: value.remote_id,
            kind: value.kind,
            source_span: value.source_span,
            content_hash: value.content_hash,
            text: value.text,
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
        }
    }
}
