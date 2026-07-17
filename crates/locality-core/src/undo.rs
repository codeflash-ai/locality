//! Connector-neutral undo planning from journaled push preimages and apply effects.
//!
//! This module does not mutate remote systems. It derives the reverse intent
//! that a connector can later apply safely. When the current journal shape does
//! not contain enough information to reverse an operation without guessing, the
//! unsupported reason is part of the plan instead of being hidden.

use std::collections::BTreeMap;

use crate::LocalityResult;
use crate::canonical::{
    ParsedCanonicalDocument, parse_canonical_markdown, render_canonical_markdown,
};
use crate::diff::property_value_from_frontmatter;
use crate::freshness::RemoteObservation;
use crate::journal::{JournalApplyEffect, JournalEntry, PushId};
use crate::model::{CanonicalDocument, MountId, RemoteId};
use crate::planner::{PropertyValue, PushOperation};
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
pub struct EntityUndoState {
    pub parent_id: RemoteId,
    pub title: String,
    pub properties: BTreeMap<String, PropertyValue>,
    pub body: String,
    pub archived: bool,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected: Option<EntityUndoState>,
    },
    RestoreEntityBody {
        entity_id: RemoteId,
        expected_current: String,
        previous: String,
    },
    RestoreProperties {
        entity_id: RemoteId,
        expected_current: BTreeMap<String, PropertyValue>,
        previous: BTreeMap<String, PropertyValue>,
    },
    RestoreEntityLocation {
        entity_id: RemoteId,
        expected_parent_id: RemoteId,
        expected_title: String,
        previous_parent_id: RemoteId,
        previous_title: String,
    },
    /// Restores the implicit guarded transition from archived=true to false.
    RestoreArchivedEntity {
        entity_id: RemoteId,
        expected: EntityUndoState,
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
                match entity_archive_expected_state(entry, entity_id) {
                    Some(expected) => {
                        operations.push(UndoOperation::RestoreArchivedEntity {
                            entity_id: entity_id.clone(),
                            expected,
                        });
                    }
                    None => unsupported.push(missing_entity_preimage(
                        operation_index,
                        entity_id,
                        "archived entity",
                    )),
                }
            }
            PushOperation::UpdateEntityBody { entity_id, body } => {
                match find_entity_preimage(entry, entity_id) {
                    Some(shadow) => operations.push(UndoOperation::RestoreEntityBody {
                        entity_id: entity_id.clone(),
                        expected_current: body.clone(),
                        previous: shadow.rendered_body.clone(),
                    }),
                    None => unsupported.push(missing_entity_preimage(
                        operation_index,
                        entity_id,
                        "body",
                    )),
                }
            }
            PushOperation::UpdateProperties {
                entity_id,
                properties,
            } => {
                if properties.is_empty() {
                    unsupported.push(UnsupportedUndoOperation::new(
                        operation_index,
                        "update_properties_missing_current_values",
                        format!(
                            "cannot restore properties for entity `{}` because the journal does not record the applied values",
                            entity_id.0
                        ),
                    ));
                    continue;
                }
                match previous_property_values(entry, entity_id, properties) {
                    Some(previous) => operations.push(UndoOperation::RestoreProperties {
                        entity_id: entity_id.clone(),
                        expected_current: properties.clone(),
                        previous,
                    }),
                    None => unsupported.push(missing_entity_preimage(
                        operation_index,
                        entity_id,
                        "property",
                    )),
                }
            }
            PushOperation::MoveEntity {
                entity_id,
                new_parent_id,
                new_title,
                ..
            } => {
                match previous_entity_location(entry, entity_id) {
                    Some((previous_parent_id, previous_title)) => {
                        operations.push(UndoOperation::RestoreEntityLocation {
                            entity_id: entity_id.clone(),
                            expected_parent_id: new_parent_id.clone(),
                            expected_title: new_title.clone(),
                            previous_parent_id,
                            previous_title,
                        });
                    }
                    None => unsupported.push(missing_entity_preimage(
                        operation_index,
                        entity_id,
                        "location",
                    )),
                }
            }
            PushOperation::CreateEntity {
                parent_id,
                title,
                properties,
                body,
                ..
            } => {
                match find_created_entity_effect(entry, operation_index) {
                    Some(entity_id) => {
                        operations.push(UndoOperation::ArchiveCreatedEntity {
                            entity_id,
                            expected: Some(EntityUndoState {
                                parent_id: parent_id.clone(),
                                title: title.clone(),
                                properties: properties.clone(),
                                body: body.clone(),
                                archived: false,
                            }),
                        });
                    }
                    None => unsupported.push(UnsupportedUndoOperation::new(
                        operation_index,
                        "create_entity_missing_created_id",
                        "cannot remove a created entity until apply journals the created remote entity id",
                    )),
                }
            }
            PushOperation::CreateDatabase {
                parent_id,
                title,
                schema,
                ..
            } => {
                match find_created_entity_effect(entry, operation_index) {
                    Some(entity_id) => {
                        operations.push(UndoOperation::ArchiveCreatedEntity {
                            entity_id,
                            expected: Some(EntityUndoState {
                                parent_id: parent_id.clone(),
                                title: title.clone(),
                                properties: BTreeMap::new(),
                                body: schema.clone(),
                                archived: false,
                            }),
                        });
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
    pub observations: Vec<RemoteObservation>,
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

fn find_entity_preimage<'a>(
    entry: &'a JournalEntry,
    entity_id: &RemoteId,
) -> Option<&'a ShadowDocument> {
    entry
        .preimages
        .iter()
        .find(|preimage| &preimage.entity_id == entity_id)
        .map(|preimage| &preimage.shadow)
}

fn parsed_entity_preimage(
    entry: &JournalEntry,
    entity_id: &RemoteId,
) -> Option<ParsedCanonicalDocument> {
    let shadow = find_entity_preimage(entry, entity_id)?;
    if shadow.frontmatter.trim().is_empty() {
        return None;
    }
    parse_canonical_markdown(&render_canonical_markdown(&CanonicalDocument::new(
        shadow.frontmatter.clone(),
        shadow.rendered_body.clone(),
    )))
    .ok()
}

fn entity_archive_expected_state(
    entry: &JournalEntry,
    entity_id: &RemoteId,
) -> Option<EntityUndoState> {
    let shadow = find_entity_preimage(entry, entity_id)?;
    let parsed = parsed_entity_preimage(entry, entity_id)?;
    let loc = parsed.frontmatter.loc?;

    if loc.id.as_ref() != Some(entity_id)
        || loc
            .entity_type
            .as_ref()
            .is_none_or(|kind| matches!(kind, crate::model::EntityKind::Unknown(_)))
    {
        return None;
    }
    let parent_id = loc.parent?;
    let title = parsed.frontmatter.title?;
    if title.trim().is_empty() {
        return None;
    }
    let properties = parsed
        .frontmatter
        .properties
        .into_iter()
        .map(|(key, value)| (key, property_value_from_frontmatter(&value)))
        .collect();
    Some(EntityUndoState {
        parent_id,
        title,
        properties,
        body: shadow.rendered_body.clone(),
        archived: true,
    })
}

fn previous_property_values(
    entry: &JournalEntry,
    entity_id: &RemoteId,
    expected_current: &BTreeMap<String, PropertyValue>,
) -> Option<BTreeMap<String, PropertyValue>> {
    let parsed = parsed_entity_preimage(entry, entity_id)?;
    Some(
        expected_current
            .keys()
            .map(|key| {
                let previous = if key == "title" {
                    parsed
                        .frontmatter
                        .title
                        .as_ref()
                        .map(|title| PropertyValue::String(title.clone()))
                } else {
                    parsed
                        .frontmatter
                        .properties
                        .get(key)
                        .map(property_value_from_frontmatter)
                }
                .unwrap_or(PropertyValue::Null);
                (key.clone(), previous)
            })
            .collect(),
    )
}

fn previous_entity_location(
    entry: &JournalEntry,
    entity_id: &RemoteId,
) -> Option<(RemoteId, String)> {
    let parsed = parsed_entity_preimage(entry, entity_id)?;
    let parent_id = parsed.frontmatter.loc?.parent?;
    let title = parsed.frontmatter.title?;
    Some((parent_id, title))
}

fn missing_entity_preimage(
    operation_index: usize,
    entity_id: &RemoteId,
    preimage_kind: &str,
) -> UnsupportedUndoOperation {
    UnsupportedUndoOperation::new(
        operation_index,
        "missing_entity_preimage",
        format!(
            "cannot restore {preimage_kind} for entity `{}` because the required journaled preimage is missing or incomplete",
            entity_id.0
        ),
    )
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
