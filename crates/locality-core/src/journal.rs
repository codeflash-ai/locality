//! Journal contracts for resumable and reversible pushes.
//!
//! The store implementation is responsible for write-ahead durability and fsync.
//! The core keeps the journal entry shape explicit so push orchestration can
//! resume or undo without connector-specific hidden state.

use crate::LocalityResult;
use crate::model::{MountId, RemoteId};
use crate::planner::{PushOperation, PushPlan};
use crate::readable_diff::ReadableDiffOutput;
use crate::shadow::ShadowDocument;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PushId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalMetadata {
    pub author: JournalAuthor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_push_id: Option<PushId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_unix_ms: Option<u128>,
}

impl Default for JournalMetadata {
    fn default() -> Self {
        Self {
            author: JournalAuthor {
                kind: JournalAuthorKind::Anonymous,
                display_name: "anonymous".to_string(),
            },
            previous_push_id: None,
            created_at_unix_ms: None,
        }
    }
}

impl JournalMetadata {
    pub fn anonymous(previous_push_id: Option<PushId>, created_at_unix_ms: Option<u128>) -> Self {
        Self {
            previous_push_id,
            created_at_unix_ms,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalAuthor {
    pub kind: JournalAuthorKind,
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalAuthorKind {
    Anonymous,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub push_id: PushId,
    pub mount_id: MountId,
    pub remote_ids: Vec<RemoteId>,
    pub plan: PushPlan,
    pub preimages: Vec<JournalPreimage>,
    pub apply_effects: Vec<JournalApplyEffect>,
    pub status: JournalStatus,
    #[serde(default)]
    pub metadata: JournalMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readable_diff: Option<ReadableDiffOutput>,
}

impl JournalEntry {
    pub fn new(
        push_id: PushId,
        mount_id: MountId,
        remote_ids: Vec<RemoteId>,
        plan: PushPlan,
        status: JournalStatus,
    ) -> Self {
        Self {
            push_id,
            mount_id,
            remote_ids,
            plan,
            preimages: Vec::new(),
            apply_effects: Vec::new(),
            status,
            metadata: JournalMetadata::default(),
            readable_diff: None,
        }
    }

    pub fn with_preimages(mut self, preimages: Vec<JournalPreimage>) -> Self {
        self.preimages = preimages;
        self
    }

    pub fn with_apply_effects(mut self, apply_effects: Vec<JournalApplyEffect>) -> Self {
        self.apply_effects = apply_effects;
        self
    }

    pub fn with_metadata(mut self, metadata: JournalMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_readable_diff(mut self, readable_diff: Option<ReadableDiffOutput>) -> Self {
        self.readable_diff = readable_diff;
        self
    }

    pub fn touches_any_entity(&self, remote_ids: &[RemoteId]) -> bool {
        self.remote_ids.iter().any(|id| remote_ids.contains(id))
            || self
                .plan
                .affected_entities
                .iter()
                .any(|id| remote_ids.contains(id))
            || self
                .preimages
                .iter()
                .any(|preimage| remote_ids.contains(&preimage.entity_id))
            || self
                .plan
                .operations
                .iter()
                .any(|operation| operation_touches_any_entity(operation, remote_ids))
            || self
                .apply_effects
                .iter()
                .any(|effect| effect_touches_any_entity(effect, remote_ids))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PushOperationId(pub String);

impl PushOperationId {
    pub fn for_operation(
        push_id: &PushId,
        operation_index: usize,
        operation: &PushOperation,
    ) -> Self {
        let target = match operation {
            PushOperation::UpdateBlock { block_id, .. }
            | PushOperation::ReplaceBlock { block_id, .. }
            | PushOperation::MoveBlock { block_id, .. }
            | PushOperation::UpdateMedia { block_id, .. }
            | PushOperation::ArchiveBlock { block_id } => block_id.0.as_str(),
            PushOperation::AppendBlock { parent_id, .. }
            | PushOperation::CreateEntity { parent_id, .. }
            | PushOperation::CreateDatabase { parent_id, .. } => parent_id.0.as_str(),
            PushOperation::ArchiveEntity { entity_id }
            | PushOperation::UpdateEntityBody { entity_id, .. }
            | PushOperation::UpdateProperties { entity_id, .. }
            | PushOperation::MoveEntity { entity_id, .. } => entity_id.0.as_str(),
        };

        Self(format!(
            "{}:{}:{}:{}",
            push_id.0,
            operation_index,
            operation_kind(operation),
            target
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalApplyEffect {
    UpdatedBlock {
        operation_id: PushOperationId,
        operation_index: usize,
        block_id: RemoteId,
    },
    CreatedBlock {
        operation_id: PushOperationId,
        operation_index: usize,
        parent_id: RemoteId,
        block_id: RemoteId,
    },
    MovedBlock {
        operation_id: PushOperationId,
        operation_index: usize,
        block_id: RemoteId,
    },
    ArchivedBlock {
        operation_id: PushOperationId,
        operation_index: usize,
        block_id: RemoteId,
    },
    ArchivedEntity {
        operation_id: PushOperationId,
        operation_index: usize,
        entity_id: RemoteId,
    },
    UpdatedEntityBody {
        operation_id: PushOperationId,
        operation_index: usize,
        entity_id: RemoteId,
    },
    UpdatedProperties {
        operation_id: PushOperationId,
        operation_index: usize,
        entity_id: RemoteId,
        keys: Vec<String>,
    },
    MovedEntity {
        operation_id: PushOperationId,
        operation_index: usize,
        entity_id: RemoteId,
        parent_id: RemoteId,
    },
    CreatedEntity {
        operation_id: PushOperationId,
        operation_index: usize,
        parent_id: RemoteId,
        entity_id: RemoteId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalPreimage {
    pub entity_id: RemoteId,
    pub shadow: ShadowDocument,
}

impl JournalPreimage {
    pub fn from_shadow(shadow: ShadowDocument) -> Self {
        Self {
            entity_id: shadow.entity_id.clone(),
            shadow,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalStatus {
    Prepared,
    Applying,
    Applied,
    Reconciled,
    Reverted,
    Failed(String),
}

impl JournalStatus {
    pub fn is_unsettled(&self) -> bool {
        matches!(
            self,
            Self::Prepared | Self::Applying | Self::Applied | Self::Failed(_)
        )
    }
}

pub trait JournalStore {
    fn append(&mut self, entry: JournalEntry) -> LocalityResult<()>;
    fn record_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> LocalityResult<()>;
    fn update_status(&mut self, push_id: &PushId, status: JournalStatus) -> LocalityResult<()>;
}

fn operation_kind(operation: &PushOperation) -> &'static str {
    match operation {
        PushOperation::UpdateBlock { .. } => "update_block",
        PushOperation::ReplaceBlock { .. } => "replace_block",
        PushOperation::AppendBlock { .. } => "append_block",
        PushOperation::MoveBlock { .. } => "move_block",
        PushOperation::UpdateMedia { .. } => "update_media",
        PushOperation::ArchiveBlock { .. } => "archive_block",
        PushOperation::ArchiveEntity { .. } => "archive_entity",
        PushOperation::UpdateEntityBody { .. } => "update_entity_body",
        PushOperation::UpdateProperties { .. } => "update_properties",
        PushOperation::MoveEntity { .. } => "move_entity",
        PushOperation::CreateEntity { .. } => "create_entity",
        PushOperation::CreateDatabase { .. } => "create_database",
    }
}

fn operation_touches_any_entity(operation: &PushOperation, remote_ids: &[RemoteId]) -> bool {
    match operation {
        PushOperation::ArchiveEntity { entity_id }
        | PushOperation::UpdateEntityBody { entity_id, .. }
        | PushOperation::UpdateProperties { entity_id, .. } => remote_ids.contains(entity_id),
        PushOperation::MoveEntity {
            entity_id,
            new_parent_id,
            ..
        } => remote_ids.contains(entity_id) || remote_ids.contains(new_parent_id),
        PushOperation::CreateEntity { parent_id, .. }
        | PushOperation::CreateDatabase { parent_id, .. }
        | PushOperation::AppendBlock { parent_id, .. } => remote_ids.contains(parent_id),
        PushOperation::UpdateBlock { .. }
        | PushOperation::ReplaceBlock { .. }
        | PushOperation::MoveBlock { .. }
        | PushOperation::UpdateMedia { .. }
        | PushOperation::ArchiveBlock { .. } => false,
    }
}

fn effect_touches_any_entity(effect: &JournalApplyEffect, remote_ids: &[RemoteId]) -> bool {
    match effect {
        JournalApplyEffect::ArchivedEntity { entity_id, .. }
        | JournalApplyEffect::UpdatedEntityBody { entity_id, .. }
        | JournalApplyEffect::UpdatedProperties { entity_id, .. } => remote_ids.contains(entity_id),
        JournalApplyEffect::MovedEntity {
            entity_id,
            parent_id,
            ..
        }
        | JournalApplyEffect::CreatedEntity {
            entity_id,
            parent_id,
            ..
        } => remote_ids.contains(entity_id) || remote_ids.contains(parent_id),
        JournalApplyEffect::CreatedBlock { parent_id, .. } => remote_ids.contains(parent_id),
        JournalApplyEffect::UpdatedBlock { .. }
        | JournalApplyEffect::MovedBlock { .. }
        | JournalApplyEffect::ArchivedBlock { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntityKind;
    use crate::planner::PushOperation;
    use crate::planner::{PlanSummary, PushPlan};
    use crate::readable_diff::ReadableDiffOutput;

    #[test]
    fn new_journal_entries_default_to_anonymous_metadata() {
        let entry = JournalEntry::new(
            PushId("push-2".to_string()),
            MountId::new("notion-main"),
            vec![RemoteId::new("page-1")],
            PushPlan {
                summary: PlanSummary::default(),
                affected_entities: vec![RemoteId::new("page-1")],
                operations: Vec::new(),
                degradations: Vec::new(),
            },
            JournalStatus::Prepared,
        );

        assert_eq!(entry.metadata.author.kind, JournalAuthorKind::Anonymous);
        assert_eq!(entry.metadata.author.display_name, "anonymous");
        assert_eq!(entry.metadata.previous_push_id, None);
        assert_eq!(entry.metadata.created_at_unix_ms, None);
        assert_eq!(entry.readable_diff, None);
    }

    #[test]
    fn journal_entry_builders_set_metadata_and_readable_diff() {
        let metadata = JournalMetadata::anonymous(Some(PushId("push-1".to_string())), Some(12_345));
        let readable_diff = ReadableDiffOutput {
            files: Vec::new(),
            text: "diff --locality a/page.md b/page.md\n".to_string(),
        };

        let entry = JournalEntry::new(
            PushId("push-2".to_string()),
            MountId::new("notion-main"),
            vec![RemoteId::new("page-1")],
            PushPlan {
                summary: PlanSummary::default(),
                affected_entities: vec![RemoteId::new("page-1")],
                operations: Vec::new(),
                degradations: Vec::new(),
            },
            JournalStatus::Prepared,
        )
        .with_metadata(metadata.clone())
        .with_readable_diff(Some(readable_diff.clone()));

        assert_eq!(entry.metadata, metadata);
        assert_eq!(entry.readable_diff, Some(readable_diff));
    }

    #[test]
    fn unsettled_statuses_include_failed_but_not_terminal_reconciliation() {
        assert!(JournalStatus::Prepared.is_unsettled());
        assert!(JournalStatus::Applying.is_unsettled());
        assert!(JournalStatus::Applied.is_unsettled());
        assert!(JournalStatus::Failed("retry".to_string()).is_unsettled());
        assert!(!JournalStatus::Reconciled.is_unsettled());
        assert!(!JournalStatus::Reverted.is_unsettled());
    }

    #[test]
    fn journal_touch_includes_operation_and_effect_parent_ids() {
        let entry = JournalEntry::new(
            PushId("push-parent-touch".to_string()),
            MountId::new("notion-main"),
            vec![],
            PushPlan::new(
                vec![],
                vec![
                    PushOperation::CreateEntity {
                        parent_id: RemoteId::new("operation-create-parent"),
                        parent_kind: None,
                        parent_workspace: false,
                        title: "Created".to_string(),
                        properties: Default::default(),
                        body: String::new(),
                        source_path: Default::default(),
                    },
                    PushOperation::MoveEntity {
                        entity_id: RemoteId::new("operation-moved-entity"),
                        new_parent_id: RemoteId::new("operation-move-parent"),
                        new_parent_kind: EntityKind::Page,
                        new_title: "Moved".to_string(),
                        projected_path: "Moved/page.md".into(),
                    },
                ],
            ),
            JournalStatus::Prepared,
        )
        .with_apply_effects(vec![
            JournalApplyEffect::CreatedEntity {
                operation_id: PushOperationId("create-effect".to_string()),
                operation_index: 0,
                parent_id: RemoteId::new("effect-create-parent"),
                entity_id: RemoteId::new("effect-created-entity"),
            },
            JournalApplyEffect::MovedEntity {
                operation_id: PushOperationId("move-effect".to_string()),
                operation_index: 1,
                entity_id: RemoteId::new("effect-moved-entity"),
                parent_id: RemoteId::new("effect-move-parent"),
            },
        ]);

        for remote_id in [
            "operation-create-parent",
            "operation-move-parent",
            "effect-create-parent",
            "effect-move-parent",
        ] {
            assert!(
                entry.touches_any_entity(&[RemoteId::new(remote_id)]),
                "journal must touch {remote_id}"
            );
        }
    }
}
