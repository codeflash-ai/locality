//! Connector-neutral undo planning from journaled push preimages and apply effects.
//!
//! This module does not mutate remote systems. It derives the reverse intent
//! that a connector can later apply safely. When the current journal shape does
//! not contain enough information to reverse an operation without guessing, the
//! unsupported reason is part of the plan instead of being hidden.

use crate::LocalityResult;
use crate::journal::{JournalApplyEffect, JournalEntry, PushId};
use crate::model::{MountId, RemoteId};
use crate::planner::PushOperation;
use crate::shadow::{ShadowBlock, ShadowDocument};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoPlan {
    pub target_push_id: PushId,
    pub mount_id: MountId,
    pub affected_entities: Vec<RemoteId>,
    pub operations: Vec<UndoOperation>,
    pub unsupported: Vec<UnsupportedUndoOperation>,
    pub status: UndoPlanStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UndoPlanStatus {
    Complete,
    Partial,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UndoOperation {
    RestoreBlockContent {
        block_id: RemoteId,
        content: String,
    },
    MoveBlock {
        block_id: RemoteId,
        after: Option<RemoteId>,
    },
    RestoreArchivedBlock {
        block_id: RemoteId,
        parent_id: RemoteId,
        after: Option<RemoteId>,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native_kind: Option<String>,
    },
    ArchiveCreatedBlock {
        block_id: RemoteId,
    },
    ArchiveCreatedEntity {
        entity_id: RemoteId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsupportedUndoOperation {
    pub operation_index: usize,
    pub code: String,
    pub message: String,
}

impl UnsupportedUndoOperation {
    fn new(operation_index: usize, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            operation_index,
            code: code.to_string(),
            message: message.into(),
        }
    }
}

pub fn plan_journal_undo(entry: &JournalEntry) -> UndoPlan {
    let mut operations = Vec::new();
    let mut unsupported = Vec::new();

    for (operation_index, operation) in entry.plan.operations.iter().enumerate().rev() {
        match operation {
            PushOperation::UpdateBlock { block_id, .. } => {
                match find_preimage_block(entry, block_id) {
                    Some((_, block)) => operations.push(UndoOperation::RestoreBlockContent {
                        block_id: block_id.clone(),
                        content: block.text.clone(),
                    }),
                    None => unsupported.push(missing_block_preimage(operation_index, block_id)),
                }
            }
            PushOperation::ReplaceBlock { block_id, .. } => {
                let created_block_id = find_created_block_effect(entry, operation_index);
                let preimage_position = find_preimage_block_position(entry, block_id);
                let preimage_block = find_preimage_block(entry, block_id);

                match (created_block_id, preimage_position, preimage_block) {
                    (Some(created_block_id), Some((shadow, after)), Some((_, block))) => {
                        operations.push(UndoOperation::ArchiveCreatedBlock {
                            block_id: created_block_id,
                        });
                        operations.push(UndoOperation::RestoreArchivedBlock {
                            block_id: block_id.clone(),
                            parent_id: shadow.entity_id.clone(),
                            after,
                            content: block.text.clone(),
                            native_kind: block.native_kind.clone(),
                        });
                    }
                    (None, _, _) => unsupported.push(UnsupportedUndoOperation::new(
                        operation_index,
                        "replace_block_missing_created_id",
                        "cannot undo a replaced block until apply journals the replacement remote block id",
                    )),
                    (_, None, _) | (_, _, None) => {
                        unsupported.push(missing_block_preimage(operation_index, block_id));
                    }
                }
            }
            PushOperation::UpdateMedia { block_id, .. } => {
                match find_preimage_block(entry, block_id) {
                    Some((_, block)) => operations.push(UndoOperation::RestoreBlockContent {
                        block_id: block_id.clone(),
                        content: block.text.clone(),
                    }),
                    None => unsupported.push(missing_block_preimage(operation_index, block_id)),
                }
            }
            PushOperation::MoveBlock { block_id, .. } => {
                let created_block_id = find_created_block_effect(entry, operation_index);
                let archived_block_id = find_archived_block_effect(entry, operation_index);
                let preimage_position = find_preimage_block_position(entry, block_id);
                let preimage_block = find_preimage_block(entry, block_id);

                match (
                    created_block_id,
                    archived_block_id,
                    preimage_position,
                    preimage_block,
                ) {
                    (Some(created_block_id), Some(archived_block_id), Some((shadow, after)), Some((_, block)))
                        if archived_block_id == *block_id =>
                    {
                        operations.push(UndoOperation::ArchiveCreatedBlock {
                            block_id: created_block_id,
                        });
                        operations.push(UndoOperation::RestoreArchivedBlock {
                            block_id: block_id.clone(),
                            parent_id: shadow.entity_id.clone(),
                            after,
                            content: block.text.clone(),
                            native_kind: block.native_kind.clone(),
                        });
                    }
                    (None, None, Some((_, after)), _) => {
                        operations.push(UndoOperation::MoveBlock {
                            block_id: block_id.clone(),
                            after,
                        });
                    }
                    (_, _, None, _) | (_, _, _, None) => {
                        unsupported.push(missing_block_preimage(operation_index, block_id));
                    }
                    _ => unsupported.push(UnsupportedUndoOperation::new(
                        operation_index,
                        "move_block_incomplete_apply_effects",
                        format!(
                            "cannot undo moved block `{}` because its journaled apply effects are incomplete",
                            block_id.0
                        ),
                    )),
                }
            }
            PushOperation::ArchiveBlock { block_id } => {
                match find_preimage_block_position(entry, block_id) {
                    Some((shadow, after)) => {
                        let Some((_, block)) = find_preimage_block(entry, block_id) else {
                            unsupported.push(missing_block_preimage(operation_index, block_id));
                            continue;
                        };
                        operations.push(UndoOperation::RestoreArchivedBlock {
                            block_id: block_id.clone(),
                            parent_id: shadow.entity_id.clone(),
                            after,
                            content: block.text.clone(),
                            native_kind: block.native_kind.clone(),
                        });
                    }
                    None => unsupported.push(missing_block_preimage(operation_index, block_id)),
                }
            }
            PushOperation::AppendBlock { .. } => {
                match find_created_block_effect(entry, operation_index) {
                    Some(block_id) => {
                        operations.push(UndoOperation::ArchiveCreatedBlock { block_id });
                    }
                    None => unsupported.push(UnsupportedUndoOperation::new(
                        operation_index,
                        "append_block_missing_created_id",
                        "cannot archive an appended block until apply journals the created remote block id",
                    )),
                }
            }
            PushOperation::ArchiveEntity { entity_id } => {
                unsupported.push(UnsupportedUndoOperation::new(
                    operation_index,
                    "archive_entity_missing_entity_preimage",
                    format!(
                        "cannot restore archived entity `{}` until entity metadata preimages are journaled",
                        entity_id.0
                    ),
                ));
            }
            PushOperation::UpdateProperties { entity_id, .. } => {
                unsupported.push(UnsupportedUndoOperation::new(
                    operation_index,
                    "update_properties_missing_property_preimage",
                    format!(
                        "cannot restore properties for entity `{}` until property preimages are journaled",
                        entity_id.0
                    ),
                ));
            }
            PushOperation::MoveEntity { entity_id, .. } => {
                unsupported.push(UnsupportedUndoOperation::new(
                    operation_index,
                    "move_entity_missing_entity_preimage",
                    format!(
                        "cannot restore moved entity `{}` until entity metadata preimages are journaled",
                        entity_id.0
                    ),
                ));
            }
            PushOperation::CreateEntity { .. } | PushOperation::CreateDatabase { .. } => {
                match find_created_entity_effect(entry, operation_index) {
                    Some(entity_id) => {
                        operations.push(UndoOperation::ArchiveCreatedEntity { entity_id });
                    }
                    None => unsupported.push(UnsupportedUndoOperation::new(
                        operation_index,
                        "create_entity_missing_created_id",
                        "cannot remove a created entity until apply journals the created remote entity id",
                    )),
                }
            }
        }
    }

    let status = match (operations.is_empty(), unsupported.is_empty()) {
        (false, true) => UndoPlanStatus::Complete,
        (false, false) => UndoPlanStatus::Partial,
        (true, _) => UndoPlanStatus::Blocked,
    };

    UndoPlan {
        target_push_id: entry.push_id.clone(),
        mount_id: entry.mount_id.clone(),
        affected_entities: entry.remote_ids.clone(),
        operations,
        unsupported,
        status,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoApplyRequest<'a> {
    pub target_push_id: &'a PushId,
    pub mount_id: &'a MountId,
    pub plan: &'a UndoPlan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoApplyResult {
    pub changed_remote_ids: Vec<RemoteId>,
}

/// Hook that applies a complete connector-neutral undo plan remotely.
pub trait UndoApplier {
    fn apply_undo(&mut self, request: UndoApplyRequest<'_>) -> LocalityResult<UndoApplyResult>;
}

fn find_preimage_block<'a>(
    entry: &'a JournalEntry,
    block_id: &RemoteId,
) -> Option<(&'a ShadowDocument, &'a ShadowBlock)> {
    entry.preimages.iter().find_map(|preimage| {
        preimage
            .shadow
            .blocks
            .iter()
            .find(|block| &block.remote_id == block_id)
            .map(|block| (&preimage.shadow, block))
    })
}

fn find_preimage_block_position<'a>(
    entry: &'a JournalEntry,
    block_id: &RemoteId,
) -> Option<(&'a ShadowDocument, Option<RemoteId>)> {
    entry.preimages.iter().find_map(|preimage| {
        let index = preimage
            .shadow
            .blocks
            .iter()
            .position(|block| &block.remote_id == block_id)?;
        let after = index
            .checked_sub(1)
            .map(|previous| preimage.shadow.blocks[previous].remote_id.clone());
        Some((&preimage.shadow, after))
    })
}

fn missing_block_preimage(operation_index: usize, block_id: &RemoteId) -> UnsupportedUndoOperation {
    UnsupportedUndoOperation::new(
        operation_index,
        "missing_block_preimage",
        format!(
            "cannot restore block `{}` because it is absent from journaled preimages",
            block_id.0
        ),
    )
}

fn find_created_block_effect(entry: &JournalEntry, operation_index: usize) -> Option<RemoteId> {
    entry.apply_effects.iter().find_map(|effect| match effect {
        JournalApplyEffect::CreatedBlock {
            operation_index: effect_operation_index,
            block_id,
            ..
        } if *effect_operation_index == operation_index => Some(block_id.clone()),
        _ => None,
    })
}

fn find_archived_block_effect(entry: &JournalEntry, operation_index: usize) -> Option<RemoteId> {
    entry.apply_effects.iter().find_map(|effect| match effect {
        JournalApplyEffect::ArchivedBlock {
            operation_index: effect_operation_index,
            block_id,
            ..
        } if *effect_operation_index == operation_index => Some(block_id.clone()),
        _ => None,
    })
}

fn find_created_entity_effect(entry: &JournalEntry, operation_index: usize) -> Option<RemoteId> {
    entry.apply_effects.iter().find_map(|effect| match effect {
        JournalApplyEffect::CreatedEntity {
            operation_index: effect_operation_index,
            entity_id,
            ..
        } if *effect_operation_index == operation_index => Some(entity_id.clone()),
        _ => None,
    })
}
