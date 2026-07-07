//! Push-plan value types and guardrail policy.
//!
//! The core describes intended remote mutations without knowing how a connector
//! executes them. Plans are inspectable before apply, and their summaries feed
//! the destructive-change guardrails from `plan.md`.

use std::collections::BTreeMap;

use crate::model::EntityKind;
use crate::model::RemoteId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushPlan {
    pub affected_entities: Vec<RemoteId>,
    pub operations: Vec<PushOperation>,
    pub summary: PlanSummary,
    pub degradations: Vec<PlanDegradation>,
}

impl PushPlan {
    pub fn new(affected_entities: Vec<RemoteId>, operations: Vec<PushOperation>) -> Self {
        let summary = PlanSummary::from_operations(&operations);
        Self {
            affected_entities,
            operations,
            summary,
            degradations: Vec::new(),
        }
    }

    pub fn with_degradations(mut self, degradations: Vec<PlanDegradation>) -> Self {
        self.degradations = degradations;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    pub fn touches_more_than_percent(&self, total_mount_entities: usize, percent: u8) -> bool {
        if total_mount_entities == 0 {
            return false;
        }

        self.affected_entities.len() * 100 > total_mount_entities * usize::from(percent)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreateParentScope {
    #[default]
    Remote,
    PrivateWorkspace,
    WorkspaceRoot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PushOperation {
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
        local_path: std::path::PathBuf,
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
    CreateEntity {
        parent_id: RemoteId,
        #[serde(default)]
        parent_kind: Option<EntityKind>,
        #[serde(default)]
        parent_scope: CreateParentScope,
        title: String,
        #[serde(default)]
        properties: BTreeMap<String, PropertyValue>,
        #[serde(default)]
        body: String,
        #[serde(default)]
        source_path: std::path::PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushOperationKind {
    UpdateBlock,
    ReplaceBlock,
    AppendBlock,
    MoveBlock,
    UpdateMedia,
    ArchiveBlock,
    ArchiveEntity,
    UpdateProperties,
    CreateEntity,
}

impl PushOperationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UpdateBlock => "update_block",
            Self::ReplaceBlock => "replace_block",
            Self::AppendBlock => "append_block",
            Self::MoveBlock => "move_block",
            Self::UpdateMedia => "update_media",
            Self::ArchiveBlock => "archive_block",
            Self::ArchiveEntity => "archive_entity",
            Self::UpdateProperties => "update_properties",
            Self::CreateEntity => "create_entity",
        }
    }

    pub fn all() -> [Self; 9] {
        [
            Self::UpdateBlock,
            Self::ReplaceBlock,
            Self::AppendBlock,
            Self::MoveBlock,
            Self::UpdateMedia,
            Self::ArchiveBlock,
            Self::ArchiveEntity,
            Self::UpdateProperties,
            Self::CreateEntity,
        ]
    }
}

impl PushOperation {
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
            Self::CreateEntity { .. } => PushOperationKind::CreateEntity,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub blocks_created: usize,
    pub blocks_updated: usize,
    #[serde(default)]
    pub blocks_replaced: usize,
    pub blocks_moved: usize,
    #[serde(default)]
    pub media_updated: usize,
    pub blocks_archived: usize,
    pub entities_created: usize,
    pub entities_archived: usize,
    pub properties_updated: usize,
}

impl PlanSummary {
    pub fn from_operations(operations: &[PushOperation]) -> Self {
        let mut summary = Self::default();

        for operation in operations {
            match operation {
                PushOperation::UpdateBlock { .. } => summary.blocks_updated += 1,
                PushOperation::ReplaceBlock { .. } => summary.blocks_replaced += 1,
                PushOperation::AppendBlock { .. } => summary.blocks_created += 1,
                PushOperation::MoveBlock { .. } => summary.blocks_moved += 1,
                PushOperation::UpdateMedia { .. } => summary.media_updated += 1,
                PushOperation::ArchiveBlock { .. } => summary.blocks_archived += 1,
                PushOperation::ArchiveEntity { .. } => summary.entities_archived += 1,
                PushOperation::UpdateProperties { properties, .. } => {
                    summary.properties_updated += properties.len();
                }
                PushOperation::CreateEntity { .. } => summary.entities_created += 1,
            }
        }

        summary
    }

    pub fn destructive_archive_count(&self) -> usize {
        self.blocks_archived + self.blocks_replaced + self.entities_archived
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PropertyValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    List(Vec<String>),
    Object(BTreeMap<String, PropertyValue>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardrailPolicy {
    pub max_archives_without_confirm: usize,
    pub max_mount_touch_percent_without_confirm: u8,
}

impl Default for GuardrailPolicy {
    fn default() -> Self {
        Self {
            max_archives_without_confirm: 10,
            max_mount_touch_percent_without_confirm: 5,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailDecision {
    Proceed,
    ConfirmRequired { reasons: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDegradation {
    pub kind: PlanDegradationKind,
    pub message: String,
}

impl PlanDegradation {
    pub fn new(kind: PlanDegradationKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanDegradationKind {
    AmbiguousBlockAlignment,
}

#[cfg(test)]
mod tests {
    use super::{CreateParentScope, PushOperation};
    use crate::model::{EntityKind, RemoteId};

    #[test]
    fn create_entity_parent_scope_defaults_to_remote_when_missing_from_json() {
        let value = serde_json::json!({
            "type": "create_entity",
            "parent_id": "page-parent",
            "parent_kind": "page",
            "title": "Child",
            "properties": {},
            "body": "",
            "source_path": "Roadmap/Child/page.md"
        });

        let operation: PushOperation =
            serde_json::from_value(value).expect("deserialize old create entity plan");

        let PushOperation::CreateEntity {
            parent_scope,
            parent_id,
            parent_kind,
            ..
        } = operation
        else {
            panic!("expected create entity");
        };
        assert_eq!(parent_scope, CreateParentScope::Remote);
        assert_eq!(parent_id, RemoteId::new("page-parent"));
        assert_eq!(parent_kind, Some(EntityKind::Page));
    }

    #[test]
    fn create_entity_parent_scope_serializes_private_workspace() {
        let operation = PushOperation::CreateEntity {
            parent_id: RemoteId::new("notion-root:private"),
            parent_kind: Some(EntityKind::Directory),
            parent_scope: CreateParentScope::PrivateWorkspace,
            title: "Private Draft".to_string(),
            properties: Default::default(),
            body: "Draft body.".to_string(),
            source_path: "Private/Private Draft/page.md".into(),
        };

        let value = serde_json::to_value(operation).expect("serialize create entity plan");

        assert_eq!(value["type"], "create_entity");
        assert_eq!(value["parent_scope"], "private_workspace");
        assert_eq!(value["parent_id"], "notion-root:private");
    }
}
