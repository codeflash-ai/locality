//! Journal contracts for resumable and reversible pushes.
//!
//! The store implementation is responsible for write-ahead durability and fsync.
//! The core keeps the journal entry shape explicit so push orchestration can
//! resume or undo without connector-specific hidden state.

use crate::AfsResult;
use crate::model::{MountId, RemoteId};
use crate::planner::{PushOperation, PushPlan};
use crate::shadow::ShadowDocument;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PushId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub push_id: PushId,
    pub mount_id: MountId,
    pub remote_ids: Vec<RemoteId>,
    pub plan: PushPlan,
    pub preimages: Vec<JournalPreimage>,
    pub apply_effects: Vec<JournalApplyEffect>,
    pub status: JournalStatus,
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
            | PushOperation::MoveBlock { block_id, .. }
            | PushOperation::UpdateMedia { block_id, .. }
            | PushOperation::ArchiveBlock { block_id } => block_id.0.as_str(),
            PushOperation::AppendBlock { parent_id, .. }
            | PushOperation::CreateEntity { parent_id, .. } => parent_id.0.as_str(),
            PushOperation::ArchiveEntity { entity_id }
            | PushOperation::UpdateProperties { entity_id, .. } => entity_id.0.as_str(),
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
    UpdatedProperties {
        operation_id: PushOperationId,
        operation_index: usize,
        entity_id: RemoteId,
        keys: Vec<String>,
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

pub trait JournalStore {
    fn append(&mut self, entry: JournalEntry) -> AfsResult<()>;
    fn record_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> AfsResult<()>;
    fn update_status(&mut self, push_id: &PushId, status: JournalStatus) -> AfsResult<()>;
}

fn operation_kind(operation: &PushOperation) -> &'static str {
    match operation {
        PushOperation::UpdateBlock { .. } => "update_block",
        PushOperation::AppendBlock { .. } => "append_block",
        PushOperation::MoveBlock { .. } => "move_block",
        PushOperation::UpdateMedia { .. } => "update_media",
        PushOperation::ArchiveBlock { .. } => "archive_block",
        PushOperation::ArchiveEntity { .. } => "archive_entity",
        PushOperation::UpdateProperties { .. } => "update_properties",
        PushOperation::CreateEntity { .. } => "create_entity",
    }
}
