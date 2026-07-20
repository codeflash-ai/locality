//! Portable source, projection, and operation values.
//!
//! These values deliberately exclude host paths, mount identity, hydration,
//! and repository row handles. Hosts bind a [`ProjectionEntry`] to those local
//! concerns only after validating its [`LogicalPath`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::path::{Component, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

use crate::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
use crate::planner::{
    PlanDegradation, PlanSummary, PropertyValue, PushOperation, PushOperationKind, PushPlan,
};

macro_rules! opaque_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

opaque_id!(TenantId);
opaque_id!(PrincipalId);
opaque_id!(SourceConnectionId);
opaque_id!(SourceVersionId);
opaque_id!(ProjectionId);
opaque_id!(ProjectionVersionId);
opaque_id!(ContentVersionId);
opaque_id!(SessionId);
opaque_id!(ReplicaRevisionId);
opaque_id!(ChangesetId);
opaque_id!(AccessSetId);

/// The only reserved export member in the portable path namespace.
///
/// The host consumes this member as writable-session metadata and never
/// exposes it in the projected tree. Other `.loc` paths remain available for
/// connector-managed artifacts such as media.
pub const RESERVED_EXPORT_METADATA_PATH: &str = ".loc/session.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalPath(String);

impl LogicalPath {
    pub fn new(value: impl Into<String>) -> Result<Self, LogicalPathError> {
        let value = value.into();
        validate_logical_path(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    /// Compatibility conversion for code that still accepts a relative
    /// `PathBuf`. This does not bind the path to a host root.
    pub fn to_relative_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }
}

impl Display for LogicalPath {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for LogicalPath {
    type Err = LogicalPathError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for LogicalPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for LogicalPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogicalPathError {
    Empty,
    Absolute,
    Backslash,
    NonNormalizedComponent(String),
    WindowsPrefix,
    ReservedMetadata,
    ReservedName(String),
    UnsafeCharacter,
}

impl Display for LogicalPathError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("logical path is empty"),
            Self::Absolute => formatter.write_str("logical path must be relative"),
            Self::Backslash => formatter.write_str("logical path must use forward slashes"),
            Self::NonNormalizedComponent(component) => {
                write!(
                    formatter,
                    "logical path contains non-normalized component `{component}`"
                )
            }
            Self::WindowsPrefix => formatter.write_str("logical path contains a Windows prefix"),
            Self::ReservedMetadata => write!(
                formatter,
                "logical path is reserved for export metadata: {RESERVED_EXPORT_METADATA_PATH}"
            ),
            Self::ReservedName(name) => {
                write!(formatter, "logical path contains reserved name `{name}`")
            }
            Self::UnsafeCharacter => {
                formatter.write_str("logical path contains an unsafe character")
            }
        }
    }
}

impl std::error::Error for LogicalPathError {}

fn validate_logical_path(value: &str) -> Result<(), LogicalPathError> {
    if value.is_empty() {
        return Err(LogicalPathError::Empty);
    }
    if value.starts_with('/') {
        return Err(LogicalPathError::Absolute);
    }
    if value.contains('\\') {
        return Err(LogicalPathError::Backslash);
    }
    if value.eq_ignore_ascii_case(RESERVED_EXPORT_METADATA_PATH) {
        return Err(LogicalPathError::ReservedMetadata);
    }

    for (index, component) in value.split('/').enumerate() {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(LogicalPathError::NonNormalizedComponent(
                component.to_string(),
            ));
        }
        if component.chars().any(char::is_control) || component.contains(':') {
            if index == 0
                && component.as_bytes().get(1) == Some(&b':')
                && component.as_bytes()[0].is_ascii_alphabetic()
            {
                return Err(LogicalPathError::WindowsPrefix);
            }
            return Err(LogicalPathError::UnsafeCharacter);
        }
        if component.ends_with(['.', ' ']) {
            return Err(LogicalPathError::NonNormalizedComponent(
                component.to_string(),
            ));
        }
        let device_name = component
            .split_once('.')
            .map_or(component, |(stem, _)| stem)
            .to_ascii_lowercase();
        if matches!(device_name.as_str(), "con" | "prn" | "aux" | "nul")
            || device_name.strip_prefix("com").is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
            || device_name.strip_prefix("lpt").is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
        {
            return Err(LogicalPathError::ReservedName(component.to_string()));
        }
    }

    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEdge {
    pub relationship: String,
    pub target_remote_id: RemoteId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AclObservation {
    pub subject_id: String,
    pub role: String,
    #[serde(default)]
    pub inherited: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceObject {
    pub source_connection_id: SourceConnectionId,
    pub remote_id: RemoteId,
    pub kind: EntityKind,
    #[serde(default)]
    pub edges: Vec<SourceEdge>,
    pub opaque_version: Option<String>,
    pub deleted: bool,
    #[serde(default)]
    pub connector_metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub acl_observations: Vec<AclObservation>,
    pub discovered_at: Option<String>,
    pub observed_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionInput {
    pub source_remote_id: RemoteId,
    pub source_version_id: SourceVersionId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionFileKind {
    Markdown,
    Text,
    Json,
    Yaml,
    Binary,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAction {
    Read,
    Search,
    DownloadAttachment,
    Create,
    Update,
    Move,
    Delete,
    Comment,
    UpdateProperties,
    ManageSchema,
}

impl SourceAction {
    pub fn all() -> [Self; 10] {
        [
            Self::Read,
            Self::Search,
            Self::DownloadAttachment,
            Self::Create,
            Self::Update,
            Self::Move,
            Self::Delete,
            Self::Comment,
            Self::UpdateProperties,
            Self::ManageSchema,
        ]
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionEntry {
    pub projection_id: ProjectionId,
    pub logical_path: LogicalPath,
    pub content_version_id: Option<ContentVersionId>,
    #[serde(default)]
    pub inputs: Vec<ProjectionInput>,
    pub file_kind: ProjectionFileKind,
    pub format_version: u32,
    #[serde(default)]
    pub supported_actions: BTreeSet<SourceAction>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalTreeEntry {
    pub projection: ProjectionEntry,
    pub mount_id: MountId,
    pub host_path: PathBuf,
    pub hydration: HydrationState,
    pub dirty: bool,
    pub local_store_id: Option<String>,
}

/// Stable values a legacy host must supply while adapting a `TreeEntry`.
///
/// No projection ID, logical path, or source-version identity is derived from
/// a mutable title or host path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyProjectionBinding {
    pub projection_id: ProjectionId,
    pub logical_path: LogicalPath,
    pub source_version_id: SourceVersionId,
    pub content_version_id: Option<ContentVersionId>,
    pub file_kind: ProjectionFileKind,
    pub format_version: u32,
    pub supported_actions: BTreeSet<SourceAction>,
    pub dirty: bool,
    pub local_store_id: Option<String>,
}

/// Lossless compatibility adapter around the existing local `TreeEntry` API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyTreeEntryAdapter {
    pub portable: LocalTreeEntry,
    legacy: TreeEntry,
}

impl LegacyTreeEntryAdapter {
    pub fn from_tree_entry(legacy: TreeEntry, binding: LegacyProjectionBinding) -> Self {
        let projection = ProjectionEntry {
            projection_id: binding.projection_id,
            logical_path: binding.logical_path,
            content_version_id: binding.content_version_id,
            inputs: vec![ProjectionInput {
                source_remote_id: legacy.remote_id.clone(),
                source_version_id: binding.source_version_id,
            }],
            file_kind: binding.file_kind,
            format_version: binding.format_version,
            supported_actions: binding.supported_actions,
        };
        let portable = LocalTreeEntry {
            projection,
            mount_id: legacy.mount_id.clone(),
            host_path: legacy.path.clone(),
            hydration: legacy.hydration.clone(),
            dirty: binding.dirty,
            local_store_id: binding.local_store_id,
        };
        Self { portable, legacy }
    }

    pub fn into_tree_entry(self) -> TreeEntry {
        self.legacy
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceOperationPlan {
    pub affected_entities: Vec<RemoteId>,
    pub operations: Vec<SourceOperation>,
    pub summary: PlanSummary,
    #[serde(default)]
    pub degradations: Vec<PlanDegradation>,
}

impl TryFrom<&PushPlan> for SourceOperationPlan {
    type Error = LogicalPathError;

    fn try_from(plan: &PushPlan) -> Result<Self, Self::Error> {
        Ok(Self {
            affected_entities: plan.affected_entities.clone(),
            operations: plan
                .operations
                .iter()
                .map(SourceOperation::try_from)
                .collect::<Result<_, _>>()?,
            summary: plan.summary.clone(),
            degradations: plan.degradations.clone(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceOperation {
    UpdateBlock {
        block_id: RemoteId,
        content: String,
    },
    ReplaceBlock {
        block_id: RemoteId,
        content: String,
    },
    AppendBlock {
        parent_id: RemoteId,
        after: Option<RemoteId>,
        content: String,
    },
    MoveBlock {
        block_id: RemoteId,
        after: Option<RemoteId>,
    },
    UpdateMedia {
        block_id: RemoteId,
        logical_path: LogicalPath,
        caption: String,
    },
    ArchiveBlock {
        block_id: RemoteId,
    },
    ArchiveEntity {
        entity_id: RemoteId,
    },
    UpdateProperties {
        entity_id: RemoteId,
        #[serde(default)]
        properties: BTreeMap<String, PropertyValue>,
    },
    MoveEntity {
        entity_id: RemoteId,
        new_parent_id: RemoteId,
        new_parent_kind: EntityKind,
        new_title: String,
        projected_path: LogicalPath,
    },
    CreateEntity {
        parent_id: RemoteId,
        #[serde(default)]
        parent_kind: Option<EntityKind>,
        #[serde(default)]
        parent_workspace: bool,
        title: String,
        #[serde(default)]
        properties: BTreeMap<String, PropertyValue>,
        #[serde(default)]
        body: String,
        source_path: LogicalPath,
    },
    CreateDatabase {
        parent_id: RemoteId,
        title: String,
        schema: String,
        source_path: LogicalPath,
    },
}

impl SourceOperation {
    pub fn kind(&self) -> PushOperationKind {
        match self {
            Self::UpdateBlock { .. } => PushOperationKind::UpdateBlock,
            Self::ReplaceBlock { .. } => PushOperationKind::ReplaceBlock,
            Self::AppendBlock { .. } => PushOperationKind::AppendBlock,
            Self::MoveBlock { .. } => PushOperationKind::MoveBlock,
            Self::UpdateMedia { .. } => PushOperationKind::UpdateMedia,
            Self::ArchiveBlock { .. } => PushOperationKind::ArchiveBlock,
            Self::ArchiveEntity { .. } => PushOperationKind::ArchiveEntity,
            Self::UpdateProperties { .. } => PushOperationKind::UpdateProperties,
            Self::MoveEntity { .. } => PushOperationKind::MoveEntity,
            Self::CreateEntity { .. } => PushOperationKind::CreateEntity,
            Self::CreateDatabase { .. } => PushOperationKind::CreateDatabase,
        }
    }
}

impl TryFrom<&PushOperation> for SourceOperation {
    type Error = LogicalPathError;

    fn try_from(operation: &PushOperation) -> Result<Self, Self::Error> {
        Ok(match operation {
            PushOperation::UpdateBlock { block_id, content } => Self::UpdateBlock {
                block_id: block_id.clone(),
                content: content.clone(),
            },
            PushOperation::ReplaceBlock { block_id, content } => Self::ReplaceBlock {
                block_id: block_id.clone(),
                content: content.clone(),
            },
            PushOperation::AppendBlock {
                parent_id,
                after,
                content,
            } => Self::AppendBlock {
                parent_id: parent_id.clone(),
                after: after.clone(),
                content: content.clone(),
            },
            PushOperation::MoveBlock { block_id, after } => Self::MoveBlock {
                block_id: block_id.clone(),
                after: after.clone(),
            },
            PushOperation::UpdateMedia {
                block_id,
                local_path,
                caption,
            } => Self::UpdateMedia {
                block_id: block_id.clone(),
                logical_path: logical_path_from_legacy(local_path)?,
                caption: caption.clone(),
            },
            PushOperation::ArchiveBlock { block_id } => Self::ArchiveBlock {
                block_id: block_id.clone(),
            },
            PushOperation::ArchiveEntity { entity_id } => Self::ArchiveEntity {
                entity_id: entity_id.clone(),
            },
            PushOperation::UpdateProperties {
                entity_id,
                properties,
            } => Self::UpdateProperties {
                entity_id: entity_id.clone(),
                properties: properties.clone(),
            },
            PushOperation::MoveEntity {
                entity_id,
                new_parent_id,
                new_parent_kind,
                new_title,
                projected_path,
            } => Self::MoveEntity {
                entity_id: entity_id.clone(),
                new_parent_id: new_parent_id.clone(),
                new_parent_kind: new_parent_kind.clone(),
                new_title: new_title.clone(),
                projected_path: logical_path_from_legacy(projected_path)?,
            },
            PushOperation::CreateEntity {
                parent_id,
                parent_kind,
                parent_workspace,
                title,
                properties,
                body,
                source_path,
            } => Self::CreateEntity {
                parent_id: parent_id.clone(),
                parent_kind: parent_kind.clone(),
                parent_workspace: *parent_workspace,
                title: title.clone(),
                properties: properties.clone(),
                body: body.clone(),
                source_path: logical_path_from_legacy(source_path)?,
            },
            PushOperation::CreateDatabase {
                parent_id,
                title,
                schema,
                source_path,
            } => Self::CreateDatabase {
                parent_id: parent_id.clone(),
                title: title.clone(),
                schema: schema.clone(),
                source_path: logical_path_from_legacy(source_path)?,
            },
        })
    }
}

fn logical_path_from_legacy(path: &std::path::Path) -> Result<LogicalPath, LogicalPathError> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => components.push(
                component
                    .to_str()
                    .ok_or(LogicalPathError::UnsafeCharacter)?,
            ),
            Component::CurDir => {
                return Err(LogicalPathError::NonNormalizedComponent(".".to_string()));
            }
            Component::ParentDir => {
                return Err(LogicalPathError::NonNormalizedComponent("..".to_string()));
            }
            Component::RootDir => return Err(LogicalPathError::Absolute),
            Component::Prefix(_) => return Err(LogicalPathError::WindowsPrefix),
        }
    }
    LogicalPath::new(components.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_path_accepts_normalized_relative_utf8() {
        let path = LogicalPath::new("Projects/日本語/page.md").expect("valid path");
        assert_eq!(path.as_str(), "Projects/日本語/page.md");
    }

    #[test]
    fn logical_path_rejects_hostile_and_reserved_values() {
        let cases = [
            "",
            "/absolute.md",
            "../escape.md",
            "safe/../../escape.md",
            "safe\\escape.md",
            "C:/windows.md",
            "safe//double.md",
            "safe/./dot.md",
            ".loc/session.json",
            ".LOC/SESSION.JSON",
            "safe/NUL.txt",
        ];

        for value in cases {
            assert!(
                LogicalPath::new(value).is_err(),
                "hostile logical path unexpectedly accepted: {value:?}"
            );
        }
    }

    #[test]
    fn logical_path_deserialization_revalidates_input() {
        let error = serde_json::from_str::<LogicalPath>(r#""../escape.md""#)
            .expect_err("deserialization must validate");
        assert!(error.to_string().contains("non-normalized"));
    }

    #[test]
    fn legacy_adapter_round_trips_without_deriving_identity() {
        let legacy = TreeEntry {
            mount_id: MountId::new("notion-main"),
            remote_id: RemoteId::new("page-1"),
            kind: EntityKind::Page,
            title: "Mutable title".to_string(),
            path: PathBuf::from("Mutable title/page.md"),
            hydration: HydrationState::Dirty,
            content_hash: Some("sha256:body".to_string()),
            remote_edited_at: Some("opaque-version".to_string()),
            stub_frontmatter: Some("title: Mutable title\n".to_string()),
        };
        let binding = LegacyProjectionBinding {
            projection_id: ProjectionId::new("projection-stable-1"),
            logical_path: LogicalPath::new("Mutable title/page.md").expect("logical path"),
            source_version_id: SourceVersionId::new("source-version-4"),
            content_version_id: Some(ContentVersionId::new("content-version-9")),
            file_kind: ProjectionFileKind::Markdown,
            format_version: 1,
            supported_actions: BTreeSet::from([SourceAction::Read, SourceAction::Update]),
            dirty: true,
            local_store_id: Some("sqlite:42".to_string()),
        };

        let adapted = LegacyTreeEntryAdapter::from_tree_entry(legacy.clone(), binding);

        assert_eq!(
            adapted.portable.projection.projection_id.as_str(),
            "projection-stable-1"
        );
        assert_eq!(
            adapted.portable.projection.inputs[0].source_remote_id,
            RemoteId::new("page-1")
        );
        assert_eq!(adapted.into_tree_entry(), legacy);
    }

    #[test]
    fn legacy_operation_paths_reject_host_paths_and_normalize_components() {
        let portable = SourceOperation::try_from(&PushOperation::CreateEntity {
            parent_id: RemoteId::new("parent-1"),
            parent_kind: Some(EntityKind::Directory),
            parent_workspace: false,
            title: "Draft".to_string(),
            properties: BTreeMap::new(),
            body: String::new(),
            source_path: ["Projects", "Draft", "page.md"].iter().collect(),
        })
        .expect("relative components are portable");
        assert!(matches!(
            portable,
            SourceOperation::CreateEntity { source_path, .. }
                if source_path.as_str() == "Projects/Draft/page.md"
        ));

        let error = SourceOperation::try_from(&PushOperation::UpdateMedia {
            block_id: RemoteId::new("block-1"),
            local_path: PathBuf::from("/tmp/private.png"),
            caption: String::new(),
        })
        .expect_err("absolute host paths cannot enter a portable plan");
        assert_eq!(error, LogicalPathError::Absolute);
    }
}
