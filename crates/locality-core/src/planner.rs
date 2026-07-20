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
    UpdateEntityBody {
        entity_id: RemoteId,
        body: String,
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
        projected_path: std::path::PathBuf,
    },
    CreateEntity {
        parent_id: RemoteId,
        #[serde(default)]
        parent_kind: Option<EntityKind>,
        #[serde(default, skip_serializing_if = "is_false")]
        parent_workspace: bool,
        title: String,
        #[serde(default)]
        properties: BTreeMap<String, PropertyValue>,
        #[serde(default)]
        body: String,
        #[serde(default)]
        source_path: std::path::PathBuf,
    },
    CreateDatabase {
        parent_id: RemoteId,
        title: String,
        schema: String,
        #[serde(default)]
        source_path: std::path::PathBuf,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
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
    UpdateEntityBody,
    UpdateProperties,
    MoveEntity,
    CreateEntity,
    CreateDatabase,
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
            Self::UpdateEntityBody => "update_entity_body",
            Self::UpdateProperties => "update_properties",
            Self::MoveEntity => "move_entity",
            Self::CreateEntity => "create_entity",
            Self::CreateDatabase => "create_database",
        }
    }

    pub fn all() -> [Self; 12] {
        [
            Self::UpdateBlock,
            Self::ReplaceBlock,
            Self::AppendBlock,
            Self::MoveBlock,
            Self::UpdateMedia,
            Self::ArchiveBlock,
            Self::ArchiveEntity,
            Self::UpdateEntityBody,
            Self::UpdateProperties,
            Self::MoveEntity,
            Self::CreateEntity,
            Self::CreateDatabase,
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
            Self::UpdateEntityBody { .. } => PushOperationKind::UpdateEntityBody,
            Self::UpdateProperties { .. } => PushOperationKind::UpdateProperties,
            Self::MoveEntity { .. } => PushOperationKind::MoveEntity,
            Self::CreateEntity { .. } => PushOperationKind::CreateEntity,
            Self::CreateDatabase { .. } => PushOperationKind::CreateDatabase,
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
    #[serde(default)]
    pub entity_bodies_updated: usize,
    #[serde(default)]
    pub entities_moved: usize,
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
                PushOperation::UpdateEntityBody { .. } => summary.entity_bodies_updated += 1,
                PushOperation::UpdateProperties { properties, .. } => {
                    summary.properties_updated += properties.len();
                }
                PushOperation::MoveEntity { .. } => summary.entities_moved += 1,
                PushOperation::CreateEntity { .. } | PushOperation::CreateDatabase { .. } => {
                    summary.entities_created += 1;
                }
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
    Array(Vec<PropertyValue>),
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
