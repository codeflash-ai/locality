//! Push-plan value types and guardrail policy.
//!
//! The core describes intended remote mutations without knowing how a connector
//! executes them. Plans are inspectable before apply, and their summaries feed
//! the destructive-change guardrails from `plan.md`.

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
pub enum PushOperation {
    UpdateBlock {
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
    ArchiveBlock {
        block_id: RemoteId,
    },
    ArchiveEntity {
        entity_id: RemoteId,
    },
    UpdateProperties {
        entity_id: RemoteId,
        keys: Vec<String>,
    },
    CreateEntity {
        parent_id: RemoteId,
        title: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub blocks_created: usize,
    pub blocks_updated: usize,
    pub blocks_moved: usize,
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
                PushOperation::AppendBlock { .. } => summary.blocks_created += 1,
                PushOperation::MoveBlock { .. } => summary.blocks_moved += 1,
                PushOperation::ArchiveBlock { .. } => summary.blocks_archived += 1,
                PushOperation::ArchiveEntity { .. } => summary.entities_archived += 1,
                PushOperation::UpdateProperties { keys, .. } => {
                    summary.properties_updated += keys.len();
                }
                PushOperation::CreateEntity { .. } => summary.entities_created += 1,
            }
        }

        summary
    }

    pub fn destructive_archive_count(&self) -> usize {
        self.blocks_archived + self.entities_archived
    }
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
pub enum PlanDegradationKind {
    AmbiguousBlockAlignment,
}
