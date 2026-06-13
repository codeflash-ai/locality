//! Apply connector-neutral push plans to Notion.
//!
//! This module is intentionally conservative. It supports Markdown blocks that
//! map cleanly to one Notion block and rich-text spans whose Markdown shape is
//! already emitted by the renderer. Unsupported content fails before making a
//! lossy request.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use afs_connector::{ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult};
use afs_core::journal::JournalApplyEffect;
use afs_core::model::RemoteId;
use afs_core::planner::{PropertyValue, PushOperation};
use afs_core::shadow::segment_markdown_body;
use afs_core::undo::{UndoOperation, UndoPlanStatus};
use afs_core::{AfsError, AfsResult};
use serde_json::{Map, Value, json};

use crate::client::NotionApi;
use crate::dto::{
    BlockDto, BlockTreeDto, DataSourceDto, NotionPageBundle, PageDto, PagePropertyDto,
    RichTextAnnotationsDto, RichTextDto, TableBlockDto,
};
use crate::fetch::fetch_page_bundle;

pub fn check_concurrency(api: &dyn NotionApi, request: ApplyPlanRequest<'_>) -> AfsResult<()> {
    let database_create_parent_ids = database_create_parent_ids(&request.plan.operations);
    for precondition in request.remote_preconditions {
        let Some(expected) = &precondition.remote_edited_at else {
            continue;
        };
        let actual = if database_create_parent_ids.contains(&precondition.remote_id) {
            api.retrieve_database(precondition.remote_id.as_str())?
                .last_edited_time
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            let page = api.retrieve_page(precondition.remote_id.as_str())?;
            page.last_edited_time
                .or(page.created_time)
                .unwrap_or_else(|| "unknown".to_string())
        };

        if actual != *expected {
            return Err(AfsError::Guardrail(format!(
                "remote entity `{}` changed since last sync (expected remote_edited_at `{expected}`, found `{actual}`)",
                precondition.remote_id.0
            )));
        }
    }

    Ok(())
}

pub fn apply_plan(
    api: &dyn NotionApi,
    request: ApplyPlanRequest<'_>,
) -> AfsResult<ApplyPlanResult> {
    validate_operation_ids(&request)?;
    let create_parent_ids = create_parent_ids(&request.plan.operations);
    let bundles = fetch_affected_bundles(api, &request.plan.affected_entities, &create_parent_ids)?;
    let current_blocks = block_map(&bundles);
    let block_parent_pages = block_parent_page_map(&bundles);
    let mut changed_remote_ids = Vec::new();
    let mut effects = Vec::new();
    let mut append_chains: BTreeMap<(RemoteId, Option<RemoteId>), RemoteId> = BTreeMap::new();

    for (operation_index, operation) in request.plan.operations.iter().enumerate() {
        match operation {
            PushOperation::UpdateBlock { block_id, content } => {
                let current = current_block(&current_blocks, block_id)?;
                if current.kind == "table" && looks_like_markdown_table(content) {
                    apply_table_update(api, &bundles, block_id, current, content)?;
                    effects.push(JournalApplyEffect::UpdatedBlock {
                        operation_id: request.operation_ids[operation_index].clone(),
                        operation_index,
                        block_id: block_id.clone(),
                    });
                    continue;
                }
                let patch = parse_supported_block(
                    content,
                    Some(current.kind.as_str()),
                    current_block_rich_text(current)?,
                )?;
                ensure_update_supported(current, &patch)?;
                api.update_block(block_id.as_str(), patch.update_body())?;
                effects.push(JournalApplyEffect::UpdatedBlock {
                    operation_id: request.operation_ids[operation_index].clone(),
                    operation_index,
                    block_id: block_id.clone(),
                });
            }
            PushOperation::AppendBlock {
                parent_id,
                after,
                content,
            } => {
                let patch = parse_supported_block(content, None, None)?;
                let chain_key = (parent_id.clone(), after.clone());
                let effective_after = append_chains
                    .get(&chain_key)
                    .cloned()
                    .or_else(|| after.clone());
                let body = append_body(patch.append_child(), effective_after.as_ref());
                let result = api.append_block_children(parent_id.as_str(), body)?;
                let created = result.results.first().ok_or_else(|| {
                    AfsError::InvalidState(
                        "notion append block children returned no created block".to_string(),
                    )
                })?;
                let created_id = RemoteId::new(created.id.clone());
                append_chains.insert(chain_key, created_id.clone());
                effects.push(JournalApplyEffect::CreatedBlock {
                    operation_id: request.operation_ids[operation_index].clone(),
                    operation_index,
                    parent_id: parent_id.clone(),
                    block_id: created_id,
                });
            }
            PushOperation::MoveBlock { block_id, after } => {
                current_block(&current_blocks, block_id)?;
                let parent_id = block_parent_pages.get(block_id).ok_or_else(|| {
                    AfsError::InvalidState(format!(
                        "push referenced block `{}` without a containing Notion page",
                        block_id.0
                    ))
                })?;
                let effective_after = append_chains
                    .get(&(parent_id.clone(), after.clone()))
                    .cloned()
                    .or_else(|| after.clone());
                api.move_block(
                    block_id.as_str(),
                    parent_id.as_str(),
                    effective_after.as_ref().map(RemoteId::as_str),
                )?;
                effects.push(JournalApplyEffect::MovedBlock {
                    operation_id: request.operation_ids[operation_index].clone(),
                    operation_index,
                    block_id: block_id.clone(),
                });
            }
            PushOperation::ArchiveBlock { block_id } => {
                api.delete_block(block_id.as_str())?;
                effects.push(JournalApplyEffect::ArchivedBlock {
                    operation_id: request.operation_ids[operation_index].clone(),
                    operation_index,
                    block_id: block_id.clone(),
                });
            }
            PushOperation::UpdateProperties {
                entity_id,
                properties,
            } => {
                let page = current_page(&bundles, entity_id)?;
                let body = update_properties_body(page, properties)?;
                api.update_page(entity_id.as_str(), body)?;
                effects.push(JournalApplyEffect::UpdatedProperties {
                    operation_id: request.operation_ids[operation_index].clone(),
                    operation_index,
                    entity_id: entity_id.clone(),
                    keys: properties.keys().cloned().collect(),
                });
            }
            PushOperation::CreateEntity {
                parent_id,
                parent_kind,
                title,
                properties,
                body,
                ..
            } => {
                let request_body = create_page_body(
                    api,
                    parent_id,
                    parent_kind.as_ref(),
                    title,
                    properties,
                    body,
                )?;
                let created = api.create_page(request_body)?;
                let created_id = RemoteId::new(created.id);
                if !changed_remote_ids.contains(&created_id) {
                    changed_remote_ids.push(created_id.clone());
                }
                effects.push(JournalApplyEffect::CreatedEntity {
                    operation_id: request.operation_ids[operation_index].clone(),
                    operation_index,
                    parent_id: parent_id.clone(),
                    entity_id: created_id,
                });
            }
            PushOperation::ArchiveEntity { entity_id } => {
                api.delete_block(entity_id.as_str())?;
                if !changed_remote_ids.contains(entity_id) {
                    changed_remote_ids.push(entity_id.clone());
                }
                effects.push(JournalApplyEffect::ArchivedEntity {
                    operation_id: request.operation_ids[operation_index].clone(),
                    operation_index,
                    entity_id: entity_id.clone(),
                });
            }
        }
    }

    for remote_id in &request.plan.affected_entities {
        if !create_parent_ids.contains(remote_id) && !changed_remote_ids.contains(remote_id) {
            changed_remote_ids.push(remote_id.clone());
        }
    }

    Ok(ApplyPlanResult {
        changed_remote_ids,
        effects,
    })
}

pub fn apply_undo(
    api: &dyn NotionApi,
    request: ApplyUndoRequest<'_>,
) -> AfsResult<ApplyUndoResult> {
    if request.plan.status != UndoPlanStatus::Complete {
        return Err(AfsError::Guardrail(
            "cannot apply an incomplete undo plan".to_string(),
        ));
    }

    for operation in &request.plan.operations {
        match operation {
            UndoOperation::RestoreBlockContent { block_id, content } => {
                if looks_like_markdown_table(content) {
                    let create_parent_ids = BTreeSet::new();
                    let bundles = fetch_affected_bundles(
                        api,
                        &request.plan.affected_entities,
                        &create_parent_ids,
                    )?;
                    let current_blocks = block_map(&bundles);
                    if let Ok(current) = current_block(&current_blocks, block_id)
                        && current.kind == "table"
                    {
                        apply_table_update(api, &bundles, block_id, current, content)?;
                        continue;
                    }
                }
                let patch = parse_supported_block(content, None, None)?;
                api.update_block(block_id.as_str(), patch.update_body())?;
            }
            UndoOperation::ArchiveCreatedBlock { block_id } => {
                api.delete_block(block_id.as_str())?;
            }
            UndoOperation::ArchiveCreatedEntity { entity_id } => {
                api.delete_block(entity_id.as_str())?;
            }
            unsupported => return Err(AfsError::Unsupported(unsupported_undo_name(unsupported))),
        }
    }

    Ok(ApplyUndoResult {
        changed_remote_ids: request.plan.affected_entities.clone(),
    })
}

fn validate_operation_ids(request: &ApplyPlanRequest<'_>) -> AfsResult<()> {
    if request.operation_ids.len() != request.plan.operations.len() {
        return Err(AfsError::InvalidState(format!(
            "push plan has {} operations but {} operation ids",
            request.plan.operations.len(),
            request.operation_ids.len()
        )));
    }

    Ok(())
}

fn create_parent_ids(operations: &[PushOperation]) -> BTreeSet<RemoteId> {
    operations
        .iter()
        .filter_map(|operation| match operation {
            PushOperation::CreateEntity { parent_id, .. } => Some(parent_id.clone()),
            _ => None,
        })
        .collect()
}

fn database_create_parent_ids(operations: &[PushOperation]) -> BTreeSet<RemoteId> {
    operations
        .iter()
        .filter_map(|operation| match operation {
            PushOperation::CreateEntity {
                parent_id,
                parent_kind,
                ..
            } if !matches!(parent_kind, Some(afs_core::model::EntityKind::Page)) => {
                Some(parent_id.clone())
            }
            _ => None,
        })
        .collect()
}

fn fetch_affected_bundles(
    api: &dyn NotionApi,
    affected_entities: &[RemoteId],
    create_parent_ids: &BTreeSet<RemoteId>,
) -> AfsResult<Vec<NotionPageBundle>> {
    affected_entities
        .iter()
        .filter(|remote_id| !create_parent_ids.contains(*remote_id))
        .map(|remote_id| fetch_page_bundle(api, remote_id.as_str()))
        .collect()
}

fn block_map(bundles: &[NotionPageBundle]) -> BTreeMap<RemoteId, &BlockDto> {
    let mut blocks = BTreeMap::new();
    for bundle in bundles {
        collect_blocks(&bundle.blocks, &mut blocks);
    }
    blocks
}

fn block_parent_page_map(bundles: &[NotionPageBundle]) -> BTreeMap<RemoteId, RemoteId> {
    let mut parents = BTreeMap::new();
    for bundle in bundles {
        collect_block_parent_pages(
            &bundle.blocks,
            &RemoteId::new(bundle.page.id.clone()),
            &mut parents,
        );
    }
    parents
}

fn collect_block_parent_pages(
    trees: &[BlockTreeDto],
    page_id: &RemoteId,
    parents: &mut BTreeMap<RemoteId, RemoteId>,
) {
    for tree in trees {
        parents.insert(RemoteId::new(tree.block.id.clone()), page_id.clone());
        collect_block_parent_pages(&tree.children, page_id, parents);
    }
}

fn collect_blocks<'a>(trees: &'a [BlockTreeDto], blocks: &mut BTreeMap<RemoteId, &'a BlockDto>) {
    for tree in trees {
        blocks.insert(RemoteId::new(tree.block.id.clone()), &tree.block);
        collect_blocks(&tree.children, blocks);
    }
}

fn apply_table_update(
    api: &dyn NotionApi,
    bundles: &[NotionPageBundle],
    table_id: &RemoteId,
    current: &BlockDto,
    markdown: &str,
) -> AfsResult<()> {
    let table = current.table.as_ref().ok_or_else(|| {
        AfsError::InvalidState(format!(
            "notion table block `{}` is missing its `table` payload",
            current.id
        ))
    })?;
    let current_rows = current_table_rows(bundles, table_id)?;
    let parsed = parse_markdown_table(markdown, table)?;

    if parsed.rows.len() != current_rows.len() {
        return Err(AfsError::Unsupported(
            "writing Notion table row additions or deletions",
        ));
    }

    for (row_block, cells) in current_rows.iter().zip(parsed.rows) {
        let current_row = row_block.table_row.as_ref().ok_or_else(|| {
            AfsError::InvalidState(format!(
                "notion table row block `{}` is missing its `table_row` payload",
                row_block.id
            ))
        })?;
        if cells.len() != current_row.cells.len() {
            return Err(AfsError::Unsupported("writing Notion table width changes"));
        }

        let cells = cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                rich_text_payload(
                    cell,
                    current_row.cells.get(index).map(|cell| cell.as_slice()),
                )
            })
            .collect::<AfsResult<Vec<_>>>()?;
        api.update_block(
            &row_block.id,
            json!({
                "table_row": {
                    "cells": cells,
                },
            }),
        )?;
    }

    Ok(())
}

fn current_table_rows<'a>(
    bundles: &'a [NotionPageBundle],
    table_id: &RemoteId,
) -> AfsResult<Vec<&'a BlockDto>> {
    let tree = bundles
        .iter()
        .find_map(|bundle| find_block_tree(&bundle.blocks, table_id))
        .ok_or_else(|| {
            AfsError::InvalidState(format!(
                "push referenced table `{}` that is absent from current Notion page content",
                table_id.0
            ))
        })?;

    tree.children
        .iter()
        .map(|child| {
            if child.block.kind == "table_row" && child.children.is_empty() {
                Ok(&child.block)
            } else {
                Err(AfsError::Unsupported(
                    "writing Notion tables with non-row children",
                ))
            }
        })
        .collect()
}

fn find_block_tree<'a>(trees: &'a [BlockTreeDto], block_id: &RemoteId) -> Option<&'a BlockTreeDto> {
    for tree in trees {
        if tree.block.id == block_id.0 {
            return Some(tree);
        }
        if let Some(found) = find_block_tree(&tree.children, block_id) {
            return Some(found);
        }
    }
    None
}

fn current_block<'a>(
    blocks: &'a BTreeMap<RemoteId, &BlockDto>,
    block_id: &RemoteId,
) -> AfsResult<&'a BlockDto> {
    blocks.get(block_id).copied().ok_or_else(|| {
        AfsError::InvalidState(format!(
            "push referenced block `{}` that is absent from current Notion page content",
            block_id.0
        ))
    })
}

fn current_page<'a>(bundles: &'a [NotionPageBundle], page_id: &RemoteId) -> AfsResult<&'a PageDto> {
    bundles
        .iter()
        .find(|bundle| bundle.page.id == page_id.0)
        .map(|bundle| &bundle.page)
        .ok_or_else(|| {
            AfsError::InvalidState(format!(
                "push referenced page `{}` that is absent from current Notion content",
                page_id.0
            ))
        })
}

fn update_properties_body(
    page: &PageDto,
    properties: &BTreeMap<String, PropertyValue>,
) -> AfsResult<Value> {
    if properties.is_empty() {
        return Err(AfsError::Unsupported(
            "applying legacy Notion property updates without values",
        ));
    }

    let mut payload = Map::new();

    for (key, value) in properties {
        let (notion_key, property) = if key == "title" {
            title_property(page)?
        } else {
            let property = page.properties.get(key).ok_or_else(|| {
                AfsError::Validation(vec![property_issue(
                    key,
                    "notion_property_unknown",
                    format!("Notion property `{key}` does not exist on the page"),
                )])
            })?;
            (key.as_str(), property)
        };
        payload.insert(
            notion_key.to_string(),
            property_update_value(property, value, key)?,
        );
    }

    Ok(json!({ "properties": Value::Object(payload) }))
}

fn create_page_body(
    api: &dyn NotionApi,
    parent_id: &RemoteId,
    parent_kind: Option<&afs_core::model::EntityKind>,
    title: &str,
    properties: &BTreeMap<String, PropertyValue>,
    body: &str,
) -> AfsResult<Value> {
    if matches!(parent_kind, Some(afs_core::model::EntityKind::Page)) {
        let mut request = json!({
            "parent": {
                "type": "page_id",
                "page_id": parent_id.0,
            },
            "properties": {
                "title": {
                    "title": rich_text(title),
                }
            },
        });
        let children = create_page_children(body)?;
        if !children.is_empty() {
            request["children"] = Value::Array(children);
        }
        return Ok(request);
    }

    let database = api.retrieve_database(parent_id.as_str())?;
    let [data_source] = database.data_sources.as_slice() else {
        return Err(AfsError::Unsupported(
            "creating Notion rows requires a database with exactly one data source",
        ));
    };
    let data_source = api.retrieve_data_source(&data_source.id)?;
    let mut request = json!({
        "parent": {
            "type": "data_source_id",
            "data_source_id": data_source.id,
        },
        "properties": create_properties_body(&data_source, title, properties)?,
    });
    let children = create_page_children(body)?;
    if !children.is_empty() {
        request["children"] = Value::Array(children);
    }

    Ok(request)
}

fn create_properties_body(
    data_source: &DataSourceDto,
    title: &str,
    properties: &BTreeMap<String, PropertyValue>,
) -> AfsResult<Value> {
    let (title_key, title_property) = data_source
        .properties
        .iter()
        .find(|(_, property)| property.kind == "title")
        .ok_or(AfsError::Unsupported(
            "creating Notion rows requires a title property",
        ))?;
    let mut payload = Map::new();
    payload.insert(
        title_key.clone(),
        property_value_for_kind(
            &title_property.kind,
            &PropertyValue::String(title.to_string()),
            "title",
        )?,
    );

    for (key, value) in properties {
        let (notion_key, property) = if key == "title" {
            (title_key, title_property)
        } else {
            let property = data_source.properties.get(key).ok_or_else(|| {
                AfsError::Validation(vec![property_issue(
                    key,
                    "notion_property_unknown",
                    format!("Notion property `{key}` does not exist on the data source"),
                )])
            })?;
            (key, property)
        };
        if notion_key == title_key {
            continue;
        }
        payload.insert(
            notion_key.clone(),
            property_value_for_kind(&property.kind, value, key)?,
        );
    }

    Ok(Value::Object(payload))
}

fn create_page_children(body: &str) -> AfsResult<Vec<Value>> {
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }

    let blocks = segment_markdown_body(body, 1);
    if blocks.len() > 100 {
        return Err(AfsError::Unsupported(
            "creating Notion pages with more than 100 initial child blocks",
        ));
    }

    blocks
        .iter()
        .map(|block| {
            if block.is_directive() {
                return Err(AfsError::Unsupported(
                    "creating Notion pages with AFS directive blocks",
                ));
            }
            parse_supported_block(&block.text, None, None).map(|patch| patch.append_child())
        })
        .collect()
}

fn title_property(page: &PageDto) -> AfsResult<(&str, &PagePropertyDto)> {
    page.properties
        .iter()
        .find(|(_, property)| property.kind == "title")
        .map(|(key, property)| (key.as_str(), property))
        .ok_or(AfsError::Unsupported(
            "updating Notion title without a title property",
        ))
}

fn property_update_value(
    property: &PagePropertyDto,
    value: &PropertyValue,
    key: &str,
) -> AfsResult<Value> {
    property_value_for_kind(&property.kind, value, key)
}

fn property_value_for_kind(kind: &str, value: &PropertyValue, key: &str) -> AfsResult<Value> {
    match kind {
        "title" => Ok(json!({ "title": rich_text(&required_string(value, key)?) })),
        "rich_text" => Ok(json!({ "rich_text": rich_text(&required_string(value, key)?) })),
        "number" => number_property(value, key),
        "select" => option_property("select", value, key),
        "status" => option_property("status", value, key),
        "multi_select" => multi_select_property(value, key),
        "checkbox" => bool_property(value, key),
        "date" => date_property(value, key),
        "url" | "email" | "phone_number" => nullable_string_property(kind, value, key),
        "files" => files_property(value, key),
        "people" => people_property(value, key),
        "relation" => relation_property(value, key),
        _ => Err(AfsError::Unsupported("updating this Notion property type")),
    }
}

fn number_property(value: &PropertyValue, key: &str) -> AfsResult<Value> {
    match value {
        PropertyValue::Null => Ok(json!({ "number": Value::Null })),
        PropertyValue::Number(value) | PropertyValue::String(value) => {
            let number = value.parse::<f64>().map_err(|_| {
                AfsError::Validation(vec![property_issue(
                    key,
                    "notion_property_number_invalid",
                    "Notion number properties must be numeric",
                )])
            })?;
            Ok(json!({ "number": number }))
        }
        _ => Err(property_type_error(key, "number")),
    }
}

fn option_property(kind: &str, value: &PropertyValue, key: &str) -> AfsResult<Value> {
    match value {
        PropertyValue::Null => Ok(single_property(kind, Value::Null)),
        PropertyValue::String(value) if value.trim().is_empty() => {
            Ok(single_property(kind, Value::Null))
        }
        PropertyValue::String(value) => Ok(single_property(kind, json!({ "name": value }))),
        _ => Err(property_type_error(key, "string or null")),
    }
}

fn multi_select_property(value: &PropertyValue, key: &str) -> AfsResult<Value> {
    match value {
        PropertyValue::Null => Ok(json!({ "multi_select": [] })),
        PropertyValue::List(values) => Ok(json!({
            "multi_select": values
                .iter()
                .map(|value| json!({ "name": value }))
                .collect::<Vec<_>>()
        })),
        PropertyValue::String(value) if value.trim().is_empty() => {
            Ok(json!({ "multi_select": [] }))
        }
        _ => Err(property_type_error(key, "list")),
    }
}

fn bool_property(value: &PropertyValue, key: &str) -> AfsResult<Value> {
    match value {
        PropertyValue::Bool(value) => Ok(json!({ "checkbox": value })),
        _ => Err(property_type_error(key, "boolean")),
    }
}

fn date_property(value: &PropertyValue, key: &str) -> AfsResult<Value> {
    match value {
        PropertyValue::Null => Ok(json!({ "date": Value::Null })),
        PropertyValue::String(value) if value.trim().is_empty() => {
            Ok(json!({ "date": Value::Null }))
        }
        PropertyValue::String(value) => Ok(json!({ "date": { "start": value } })),
        PropertyValue::Object(fields) => {
            let start = match fields.get("start") {
                Some(PropertyValue::String(value)) => value,
                _ => return Err(property_type_error(key, "date object with string start")),
            };
            let mut date = json!({ "start": start });
            if let Some(end) = fields.get("end").and_then(property_string) {
                date["end"] = json!(end);
            }
            if let Some(time_zone) = fields.get("time_zone").and_then(property_string) {
                date["time_zone"] = json!(time_zone);
            }
            Ok(json!({ "date": date }))
        }
        _ => Err(property_type_error(key, "date string or object")),
    }
}

fn nullable_string_property(kind: &str, value: &PropertyValue, key: &str) -> AfsResult<Value> {
    match value {
        PropertyValue::Null => Ok(single_property(kind, Value::Null)),
        PropertyValue::String(value) if value.trim().is_empty() => {
            Ok(single_property(kind, Value::Null))
        }
        PropertyValue::String(value) => Ok(single_property(kind, json!(value))),
        _ => Err(property_type_error(key, "string or null")),
    }
}

fn single_property(kind: &str, value: Value) -> Value {
    let mut object = Map::new();
    object.insert(kind.to_string(), value);
    Value::Object(object)
}

fn files_property(value: &PropertyValue, key: &str) -> AfsResult<Value> {
    let entries = match value {
        PropertyValue::Null => Vec::new(),
        PropertyValue::String(value) if value.trim().is_empty() => Vec::new(),
        PropertyValue::String(value) => vec![value.as_str()],
        PropertyValue::List(values) => values.iter().map(String::as_str).collect(),
        _ => return Err(property_type_error(key, "file URL string or list")),
    };

    let files = entries
        .into_iter()
        .map(|entry| external_file_property_value(entry, key))
        .collect::<AfsResult<Vec<_>>>()?;
    Ok(json!({ "files": files }))
}

fn external_file_property_value(entry: &str, key: &str) -> AfsResult<Value> {
    let (name, url) = parse_external_file_entry(entry);
    if url.trim().is_empty() || !valid_url(url) {
        return Err(AfsError::Validation(vec![property_issue(
            key,
            "notion_property_file_url_invalid",
            "Notion file properties must be HTTP(S) URLs or `name <url>` entries",
        )]));
    }
    let name = if name.trim().is_empty() {
        file_name_from_url(url)
    } else {
        name.trim().to_string()
    };

    Ok(json!({
        "name": name,
        "type": "external",
        "external": {
            "url": url,
        },
    }))
}

fn parse_external_file_entry(entry: &str) -> (&str, &str) {
    let trimmed = entry.trim();
    if let Some(without_close) = trimmed.strip_suffix('>')
        && let Some((name, url)) = without_close.rsplit_once(" <")
    {
        return (name, url);
    }
    ("", trimmed)
}

fn file_name_from_url(url: &str) -> String {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or("File")
        .to_string()
}

fn valid_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn people_property(value: &PropertyValue, key: &str) -> AfsResult<Value> {
    let entries = match value {
        PropertyValue::Null => Vec::new(),
        PropertyValue::String(value) if value.trim().is_empty() => Vec::new(),
        PropertyValue::String(value) => vec![value.as_str()],
        PropertyValue::List(values) => values.iter().map(String::as_str).collect(),
        _ => return Err(property_type_error(key, "Notion user ID string or list")),
    };

    let people = entries
        .into_iter()
        .map(|entry| people_property_value(entry, key))
        .collect::<AfsResult<Vec<_>>>()?;
    Ok(json!({ "people": people }))
}

fn people_property_value(entry: &str, key: &str) -> AfsResult<Value> {
    let id = parse_named_id_entry(entry).trim();
    if !valid_notion_id(id) {
        return Err(AfsError::Validation(vec![property_issue(
            key,
            "notion_property_people_id_invalid",
            "Notion people properties must contain user IDs",
        )]));
    }

    Ok(json!({ "id": id }))
}

fn relation_property(value: &PropertyValue, key: &str) -> AfsResult<Value> {
    let entries = match value {
        PropertyValue::Null => Vec::new(),
        PropertyValue::String(value) if value.trim().is_empty() => Vec::new(),
        PropertyValue::String(value) => vec![value.as_str()],
        PropertyValue::List(values) => values.iter().map(String::as_str).collect(),
        _ => return Err(property_type_error(key, "Notion page ID string or list")),
    };

    let relations = entries
        .into_iter()
        .map(|entry| relation_property_value(entry, key))
        .collect::<AfsResult<Vec<_>>>()?;
    Ok(json!({ "relation": relations }))
}

fn relation_property_value(entry: &str, key: &str) -> AfsResult<Value> {
    let id = parse_named_id_entry(entry).trim();
    if !valid_notion_id(id) {
        return Err(AfsError::Validation(vec![property_issue(
            key,
            "notion_property_relation_id_invalid",
            "Notion relation properties must contain page IDs",
        )]));
    }

    Ok(json!({ "id": id }))
}

fn parse_named_id_entry(entry: &str) -> &str {
    let trimmed = entry.trim();
    if let Some(without_close) = trimmed.strip_suffix('>')
        && let Some((_, id)) = without_close.rsplit_once(" <")
    {
        return id;
    }
    trimmed
}

fn valid_notion_id(value: &str) -> bool {
    let compact = value.replace('-', "");
    compact.len() == 32 && compact.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn required_string(value: &PropertyValue, key: &str) -> AfsResult<String> {
    match value {
        PropertyValue::String(value) => Ok(value.clone()),
        _ => Err(property_type_error(key, "string")),
    }
}

fn property_string(value: &PropertyValue) -> Option<&str> {
    match value {
        PropertyValue::String(value) => Some(value),
        _ => None,
    }
}

fn property_type_error(key: &str, expected: &str) -> AfsError {
    AfsError::Validation(vec![property_issue(
        key,
        "notion_property_type_mismatch",
        format!("Notion property `{key}` must be {expected}"),
    )])
}

fn property_issue(
    key: &str,
    code: impl Into<String>,
    message: impl Into<String>,
) -> afs_core::validation::ValidationIssue {
    afs_core::validation::ValidationIssue::new(
        code,
        "",
        None,
        message,
        Some(format!(
            "restore `{key}` to a value compatible with the database schema"
        )),
    )
}

fn ensure_update_supported(current: &BlockDto, patch: &NotionBlockPatch) -> AfsResult<()> {
    if current.kind != patch.kind {
        return Err(AfsError::Unsupported("changing Notion block type"));
    }

    Ok(())
}

fn current_block_rich_text(block: &BlockDto) -> AfsResult<Option<&[RichTextDto]>> {
    let rich_text = match block.kind.as_str() {
        "paragraph" => block
            .paragraph
            .as_ref()
            .map(|block| block.rich_text.as_slice()),
        "heading_1" => block
            .heading_1
            .as_ref()
            .map(|block| block.rich_text.as_slice()),
        "heading_2" => block
            .heading_2
            .as_ref()
            .map(|block| block.rich_text.as_slice()),
        "heading_3" => block
            .heading_3
            .as_ref()
            .map(|block| block.rich_text.as_slice()),
        "heading_4" => block
            .heading_4
            .as_ref()
            .map(|block| block.rich_text.as_slice()),
        "toggle" => block
            .toggle
            .as_ref()
            .map(|block| block.rich_text.as_slice()),
        "bulleted_list_item" => block
            .bulleted_list_item
            .as_ref()
            .map(|block| block.rich_text.as_slice()),
        "numbered_list_item" => block
            .numbered_list_item
            .as_ref()
            .map(|block| block.rich_text.as_slice()),
        "quote" => block.quote.as_ref().map(|block| block.rich_text.as_slice()),
        "callout" => block
            .callout
            .as_ref()
            .map(|block| block.rich_text.as_slice()),
        "to_do" => block.to_do.as_ref().map(|block| block.rich_text.as_slice()),
        "code" => block.code.as_ref().map(|block| block.rich_text.as_slice()),
        "bookmark" => block
            .bookmark
            .as_ref()
            .map(|block| block.caption.as_slice()),
        "embed" => block.embed.as_ref().map(|block| block.caption.as_slice()),
        "image" => block.image.as_ref().map(|block| block.caption.as_slice()),
        "video" => block.video.as_ref().map(|block| block.caption.as_slice()),
        "file" => block.file.as_ref().map(|block| block.caption.as_slice()),
        "pdf" => block.pdf.as_ref().map(|block| block.caption.as_slice()),
        "audio" => block.audio.as_ref().map(|block| block.caption.as_slice()),
        "divider" | "equation" => return Ok(None),
        _ => return Ok(None),
    }
    .ok_or_else(|| {
        AfsError::InvalidState(format!(
            "notion block `{}` is missing its `{}` payload",
            block.id, block.kind
        ))
    })?;

    Ok(Some(rich_text))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NotionBlockPatch {
    kind: &'static str,
    payload: Value,
}

impl NotionBlockPatch {
    fn new(kind: &'static str, payload: Value) -> Self {
        Self { kind, payload }
    }

    fn update_body(&self) -> Value {
        json!({ self.kind: self.payload.clone() })
    }

    fn append_child(&self) -> Value {
        let mut object = Map::new();
        object.insert("object".to_string(), json!("block"));
        object.insert("type".to_string(), json!(self.kind));
        object.insert(self.kind.to_string(), self.payload.clone());
        Value::Object(object)
    }
}

struct ParsedMarkdownTable {
    rows: Vec<Vec<String>>,
}

fn parse_markdown_table(
    markdown: &str,
    current_table: &TableBlockDto,
) -> AfsResult<ParsedMarkdownTable> {
    let lines = markdown
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if lines.len() < 2 {
        return Err(AfsError::Unsupported("writing malformed Notion tables"));
    }

    let header = parse_markdown_table_row(lines[0])?;
    validate_markdown_table_separator(lines[1], header.len())?;
    let mut data_rows = lines[2..]
        .iter()
        .map(|line| parse_markdown_table_row(line))
        .collect::<AfsResult<Vec<_>>>()?;
    let width = usize::from(current_table.table_width);
    if width == 0 || header.len() != width || data_rows.iter().any(|row| row.len() != width) {
        return Err(AfsError::Unsupported("writing Notion table width changes"));
    }

    let rows = if current_table.has_column_header {
        let mut rows = Vec::with_capacity(data_rows.len() + 1);
        rows.push(header);
        rows.append(&mut data_rows);
        rows
    } else {
        if header.iter().any(|cell| !cell.trim().is_empty()) {
            return Err(AfsError::Unsupported(
                "writing Notion table header-mode changes",
            ));
        }
        data_rows
    };

    Ok(ParsedMarkdownTable { rows })
}

fn parse_markdown_table_row(line: &str) -> AfsResult<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') || !trimmed.ends_with('|') || trimmed.len() < 2 {
        return Err(AfsError::Unsupported("writing malformed Notion tables"));
    }

    let inner = &trimmed[1..trimmed.len() - 1];
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in inner.chars() {
        if ch == '|' && !escaped {
            cells.push(unescape_markdown_table_cell(current.trim()));
            current.clear();
        } else {
            current.push(ch);
        }
        escaped = ch == '\\' && !escaped;
        if ch != '\\' {
            escaped = false;
        }
    }
    cells.push(unescape_markdown_table_cell(current.trim()));

    Ok(cells)
}

fn validate_markdown_table_separator(line: &str, width: usize) -> AfsResult<()> {
    let cells = parse_markdown_table_row(line)?;
    let valid = cells.len() == width
        && cells.iter().all(|cell| {
            let trimmed = cell.trim();
            trimmed.contains('-') && trimmed.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
        });
    if valid {
        Ok(())
    } else {
        Err(AfsError::Unsupported("writing malformed Notion tables"))
    }
}

fn unescape_markdown_table_cell(cell: &str) -> String {
    cell.replace("\\|", "|").replace("<br>", "\n")
}

fn parse_supported_block(
    markdown: &str,
    current_kind: Option<&str>,
    preimage: Option<&[RichTextDto]>,
) -> AfsResult<NotionBlockPatch> {
    let trimmed = markdown.trim_end_matches('\n');

    if trimmed.trim().is_empty() {
        return Err(AfsError::Unsupported("empty Notion block writes"));
    }

    if let Some((language, code)) = parse_code_fence(trimmed) {
        let language = if language.is_empty() {
            "plain text".to_string()
        } else {
            language
        };
        return Ok(NotionBlockPatch::new(
            "code",
            json!({
                "rich_text": rich_text(&code),
                "language": language,
            }),
        ));
    }

    if trimmed == "---" {
        return Ok(NotionBlockPatch::new("divider", json!({})));
    }

    if let Some(expression) = parse_display_equation(trimmed) {
        return Ok(NotionBlockPatch::new(
            "equation",
            json!({ "expression": expression }),
        ));
    }

    if let Some((level, text)) = parse_heading(trimmed) {
        let kind = match level {
            1 => "heading_1",
            2 => "heading_2",
            3 => "heading_3",
            4 => "heading_4",
            _ => return Err(AfsError::Unsupported("Notion heading levels above 4")),
        };
        return Ok(NotionBlockPatch::new(
            kind,
            json!({ "rich_text": rich_text_payload(text, preimage)? }),
        ));
    }

    if let Some((checked, text)) = parse_to_do(trimmed) {
        return Ok(NotionBlockPatch::new(
            "to_do",
            json!({
                "rich_text": rich_text_payload(text, preimage)?,
                "checked": checked,
            }),
        ));
    }

    if let Some(text) = parse_bulleted_list_item(trimmed) {
        let kind = if current_kind == Some("toggle") {
            "toggle"
        } else {
            "bulleted_list_item"
        };
        return Ok(NotionBlockPatch::new(
            kind,
            json!({ "rich_text": rich_text_payload(text, preimage)? }),
        ));
    }

    if let Some(text) = parse_numbered_list_item(trimmed) {
        return Ok(NotionBlockPatch::new(
            "numbered_list_item",
            json!({ "rich_text": rich_text_payload(text, preimage)? }),
        ));
    }

    if let Some(text) = parse_callout(trimmed) {
        return Ok(NotionBlockPatch::new(
            "callout",
            json!({ "rich_text": rich_text_payload(&text, preimage)? }),
        ));
    }

    if let Some(text) = parse_quote(trimmed) {
        return Ok(NotionBlockPatch::new(
            "quote",
            json!({ "rich_text": rich_text_payload(&text, preimage)? }),
        ));
    }

    if let Some(kind @ ("bookmark" | "embed")) = current_kind
        && let Some((label, href, consumed)) = parse_markdown_link(trimmed)
        && consumed == trimmed.len()
    {
        let kind = match kind {
            "bookmark" => "bookmark",
            "embed" => "embed",
            _ => unreachable!("matched URL block kind"),
        };
        return Ok(NotionBlockPatch::new(
            kind,
            json!({
                "url": href,
                "caption": rich_text_payload(label, preimage)?,
            }),
        ));
    }

    if let Some(kind @ ("image" | "video" | "file" | "pdf" | "audio")) = current_kind
        && let Some((label, href)) = parse_media_markdown(kind, trimmed)
    {
        let kind = match kind {
            "image" => "image",
            "video" => "video",
            "file" => "file",
            "pdf" => "pdf",
            "audio" => "audio",
            _ => unreachable!("matched media block kind"),
        };
        return Ok(NotionBlockPatch::new(
            kind,
            json!({
                "external": {
                    "url": href,
                },
                "caption": rich_text_payload(label, preimage)?,
            }),
        ));
    }

    if looks_like_markdown_table(trimmed) {
        return Err(AfsError::Unsupported("writing Notion tables"));
    }

    Ok(NotionBlockPatch::new(
        "paragraph",
        json!({ "rich_text": rich_text_payload(trimmed, preimage)? }),
    ))
}

fn append_body(child: Value, after: Option<&RemoteId>) -> Value {
    match after {
        Some(after) => json!({
            "children": [child],
            "position": {
                "type": "after_block",
                "after_block": {
                    "id": after.0,
                },
            },
        }),
        None => json!({
            "children": [child],
            "position": {
                "type": "start",
            },
        }),
    }
}

fn rich_text(content: &str) -> Value {
    json!([
        {
            "type": "text",
            "text": {
                "content": content,
            },
        }
    ])
}

fn rich_text_payload(content: &str, preimage: Option<&[RichTextDto]>) -> AfsResult<Value> {
    let parts = parse_rich_text_markdown(content, preimage)?;
    Ok(Value::Array(
        parts
            .iter()
            .map(RichTextWritePart::to_request_value)
            .collect::<AfsResult<Vec<_>>>()?,
    ))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct InlineAnnotations {
    bold: bool,
    italic: bool,
    strikethrough: bool,
    underline: bool,
    code: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RichTextWritePart {
    Text {
        content: String,
        link: Option<String>,
        annotations: InlineAnnotations,
    },
    Equation {
        expression: String,
        annotations: InlineAnnotations,
    },
    PageMention {
        id: String,
        annotations: InlineAnnotations,
    },
    DatabaseMention {
        id: String,
        annotations: InlineAnnotations,
    },
    Preimage(Box<RichTextDto>),
}

impl RichTextWritePart {
    fn apply_annotation(&mut self, apply: impl FnOnce(&mut InlineAnnotations)) {
        match self {
            Self::Text { annotations, .. }
            | Self::Equation { annotations, .. }
            | Self::PageMention { annotations, .. }
            | Self::DatabaseMention { annotations, .. } => apply(annotations),
            Self::Preimage(part) => {
                let mut annotations = InlineAnnotations::from(&part.annotations);
                apply(&mut annotations);
                part.annotations = RichTextAnnotationsDto::from(annotations);
            }
        }
    }

    fn apply_link(&mut self, href: &str) -> AfsResult<()> {
        match self {
            Self::Text { link, .. } => {
                *link = Some(href.to_string());
                Ok(())
            }
            Self::Preimage(part) if part.kind == "text" || part.kind.is_empty() => {
                part.href = Some(href.to_string());
                if let Some(text) = part.text.as_mut() {
                    text.link = Some(crate::dto::LinkDto {
                        url: href.to_string(),
                    });
                }
                Ok(())
            }
            _ => Err(AfsError::Unsupported("links on non-text rich spans")),
        }
    }

    fn to_request_value(&self) -> AfsResult<Value> {
        match self {
            Self::Text {
                content,
                link,
                annotations,
            } => {
                let mut text = json!({ "content": content });
                if let Some(link) = link {
                    text["link"] = json!({ "url": link });
                }
                let mut value = json!({
                    "type": "text",
                    "text": text,
                });
                insert_annotations(&mut value, annotations);
                Ok(value)
            }
            Self::Equation {
                expression,
                annotations,
            } => {
                let mut value = json!({
                    "type": "equation",
                    "equation": {
                        "expression": expression,
                    },
                });
                insert_annotations(&mut value, annotations);
                Ok(value)
            }
            Self::PageMention { id, annotations } => {
                let mut value = json!({
                    "type": "mention",
                    "mention": {
                        "type": "page",
                        "page": {
                            "id": id,
                        },
                    },
                });
                insert_annotations(&mut value, annotations);
                Ok(value)
            }
            Self::DatabaseMention { id, annotations } => {
                let mut value = json!({
                    "type": "mention",
                    "mention": {
                        "type": "database",
                        "database": {
                            "id": id,
                        },
                    },
                });
                insert_annotations(&mut value, annotations);
                Ok(value)
            }
            Self::Preimage(part) => preimage_part_to_request_value(part),
        }
    }
}

impl From<&RichTextAnnotationsDto> for InlineAnnotations {
    fn from(value: &RichTextAnnotationsDto) -> Self {
        Self {
            bold: value.bold,
            italic: value.italic,
            strikethrough: value.strikethrough,
            underline: value.underline,
            code: value.code,
        }
    }
}

impl From<InlineAnnotations> for RichTextAnnotationsDto {
    fn from(value: InlineAnnotations) -> Self {
        Self {
            bold: value.bold,
            italic: value.italic,
            strikethrough: value.strikethrough,
            underline: value.underline,
            code: value.code,
            color: None,
        }
    }
}

fn parse_rich_text_markdown(
    content: &str,
    preimage: Option<&[RichTextDto]>,
) -> AfsResult<Vec<RichTextWritePart>> {
    let preimage_tokens = preimage.map(preimage_tokens).unwrap_or_default();
    let mut parser = InlineParser {
        input: content,
        preimage_tokens: &preimage_tokens,
        allow_preimage: true,
    };
    parser.parse_until(None)
}

#[derive(Clone, Debug)]
struct PreimageToken {
    markdown: String,
    part: RichTextDto,
}

fn preimage_tokens(parts: &[RichTextDto]) -> Vec<PreimageToken> {
    let mut tokens = parts
        .iter()
        .filter_map(|part| {
            let markdown = render_rich_text_part_for_match(part);
            if markdown.is_empty() {
                None
            } else {
                Some(PreimageToken {
                    markdown,
                    part: part.clone(),
                })
            }
        })
        .collect::<Vec<_>>();
    tokens.sort_by_key(|token| Reverse(token.markdown.len()));
    tokens
}

struct InlineParser<'a> {
    input: &'a str,
    preimage_tokens: &'a [PreimageToken],
    allow_preimage: bool,
}

impl InlineParser<'_> {
    fn parse_until(&mut self, closing: Option<&str>) -> AfsResult<Vec<RichTextWritePart>> {
        let mut parts = Vec::new();
        let mut index = 0;

        while index < self.input.len() {
            if let Some(closing) = closing
                && self.input[index..].starts_with(closing)
            {
                break;
            }

            if self.allow_preimage
                && let Some(token) = self.match_preimage(index)
            {
                parts.push(RichTextWritePart::Preimage(Box::new(token.part.clone())));
                index += token.markdown.len();
                continue;
            }

            if let Some((part, consumed)) = self.parse_special(index)? {
                parts.extend(part);
                index += consumed;
                continue;
            }

            let next = self.next_special_or_preimage(index + 1, closing);
            parts.push(RichTextWritePart::Text {
                content: unescape_markdown_text(&self.input[index..next]),
                link: None,
                annotations: InlineAnnotations::default(),
            });
            index = next;
        }

        Ok(parts)
    }

    fn match_preimage(&self, index: usize) -> Option<&PreimageToken> {
        self.preimage_tokens
            .iter()
            .find(|token| self.input[index..].starts_with(&token.markdown))
    }

    fn parse_special(&self, index: usize) -> AfsResult<Option<(Vec<RichTextWritePart>, usize)>> {
        let rest = &self.input[index..];

        if rest.starts_with("**")
            && let Some(end) = find_closing(rest, 2, "**")
        {
            let mut parts = parse_nested(&rest[2..end], self.preimage_tokens, false)?;
            for part in &mut parts {
                part.apply_annotation(|annotations| annotations.bold = true);
            }
            return Ok(Some((parts, end + 2)));
        }

        if rest.starts_with("~~")
            && let Some(end) = find_closing(rest, 2, "~~")
        {
            let mut parts = parse_nested(&rest[2..end], self.preimage_tokens, false)?;
            for part in &mut parts {
                part.apply_annotation(|annotations| annotations.strikethrough = true);
            }
            return Ok(Some((parts, end + 2)));
        }

        if rest.starts_with("<u>")
            && let Some(end) = rest[3..].find("</u>").map(|offset| offset + 3)
        {
            let mut parts = parse_nested(&rest[3..end], self.preimage_tokens, false)?;
            for part in &mut parts {
                part.apply_annotation(|annotations| annotations.underline = true);
            }
            return Ok(Some((parts, end + 4)));
        }

        if rest.starts_with('`')
            && let Some(end) = find_closing(rest, 1, "`")
        {
            return Ok(Some((
                vec![RichTextWritePart::Text {
                    content: rest[1..end].replace("\\`", "`"),
                    link: None,
                    annotations: InlineAnnotations {
                        code: true,
                        ..Default::default()
                    },
                }],
                end + 1,
            )));
        }

        if rest.starts_with('$')
            && let Some(end) = find_closing(rest, 1, "$")
        {
            return Ok(Some((
                vec![RichTextWritePart::Equation {
                    expression: rest[1..end].replace("\\$", "$"),
                    annotations: InlineAnnotations::default(),
                }],
                end + 1,
            )));
        }

        if rest.starts_with('[')
            && let Some((label, href, consumed)) = parse_markdown_link(rest)
        {
            if let Some(id) = notion_page_id_from_href(href) {
                if self.preimage_has_mention("database", &id) {
                    return Ok(Some((
                        vec![RichTextWritePart::DatabaseMention {
                            id,
                            annotations: InlineAnnotations::default(),
                        }],
                        consumed,
                    )));
                }
                return Ok(Some((
                    vec![RichTextWritePart::PageMention {
                        id,
                        annotations: InlineAnnotations::default(),
                    }],
                    consumed,
                )));
            }

            let mut parts = parse_nested(label, self.preimage_tokens, false)?;
            for part in &mut parts {
                part.apply_link(href)?;
            }
            return Ok(Some((parts, consumed)));
        }

        if rest.starts_with('_')
            && let Some(end) = find_closing(rest, 1, "_")
        {
            let mut parts = parse_nested(&rest[1..end], self.preimage_tokens, false)?;
            for part in &mut parts {
                part.apply_annotation(|annotations| annotations.italic = true);
            }
            return Ok(Some((parts, end + 1)));
        }

        Ok(None)
    }

    fn preimage_has_mention(&self, kind: &str, id: &str) -> bool {
        self.preimage_tokens.iter().any(|token| {
            let Some(mention) = token.part.mention.as_ref() else {
                return false;
            };
            if mention.kind != kind {
                return false;
            }
            let preimage_id = match kind {
                "page" => mention.page.as_ref().map(|page| page.id.as_str()),
                "database" => mention
                    .database
                    .as_ref()
                    .map(|database| database.id.as_str()),
                _ => None,
            };
            preimage_id.is_some_and(|preimage_id| notion_ids_equal(preimage_id, id))
        })
    }

    fn next_special_or_preimage(&self, start: usize, closing: Option<&str>) -> usize {
        let mut next = self.input.len();
        for marker in ["**", "~~", "<u>", "`", "$", "[", "_"] {
            if let Some(offset) = self.input[start..].find(marker) {
                next = next.min(start + offset);
            }
        }
        if let Some(closing) = closing
            && let Some(offset) = self.input[start..].find(closing)
        {
            next = next.min(start + offset);
        }
        if self.allow_preimage {
            for token in self.preimage_tokens {
                if let Some(offset) = self.input[start..].find(&token.markdown) {
                    next = next.min(start + offset);
                }
            }
        }
        next
    }
}

fn parse_nested(
    input: &str,
    preimage_tokens: &[PreimageToken],
    allow_preimage: bool,
) -> AfsResult<Vec<RichTextWritePart>> {
    let mut parser = InlineParser {
        input,
        preimage_tokens,
        allow_preimage,
    };
    parser.parse_until(None)
}

fn find_closing(input: &str, start: usize, marker: &str) -> Option<usize> {
    input[start..].find(marker).map(|offset| start + offset)
}

fn parse_markdown_link(input: &str) -> Option<(&str, &str, usize)> {
    if !input.starts_with('[') {
        return None;
    }
    let label_end = input.find("](")?;
    let href_start = label_end + 2;
    let href_end = input[href_start..]
        .find(')')
        .map(|offset| href_start + offset)?;
    Some((
        &input[1..label_end],
        &input[href_start..href_end],
        href_end + 1,
    ))
}

fn parse_media_markdown<'a>(kind: &str, input: &'a str) -> Option<(&'a str, &'a str)> {
    let (label, href, consumed) = match kind {
        "image" => {
            let link = input.strip_prefix('!')?;
            let (label, href, consumed) = parse_markdown_link(link)?;
            (label, href, consumed + 1)
        }
        "video" | "file" | "pdf" | "audio" => parse_markdown_link(input)?,
        _ => return None,
    };

    if consumed == input.len() {
        Some((label, href))
    } else {
        None
    }
}

fn notion_page_id_from_href(href: &str) -> Option<String> {
    if let Some(id) = href.strip_prefix("afs://") {
        return Some(id.to_string());
    }

    let trimmed = href.trim();
    if !is_notion_url(trimmed) {
        return None;
    }

    let without_query = trimmed
        .split(['?', '#'])
        .next()
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    without_query
        .rsplit('/')
        .find_map(notion_id_from_url_segment)
}

fn is_notion_url(href: &str) -> bool {
    let lower = href.to_ascii_lowercase();
    lower.starts_with("https://www.notion.so/")
        || lower.starts_with("https://notion.so/")
        || lower.starts_with("https://app.notion.com/")
}

fn notion_id_from_url_segment(segment: &str) -> Option<String> {
    if segment.is_empty() {
        return None;
    }

    let without_hyphens = segment.replace('-', "");
    if without_hyphens.len() == 32
        && without_hyphens
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Some(without_hyphens);
    }

    let trailing_hex = segment
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_hexdigit())
        .collect::<Vec<_>>();
    if trailing_hex.len() >= 32 {
        return Some(trailing_hex.iter().take(32).rev().copied().collect());
    }

    None
}

fn notion_ids_equal(left: &str, right: &str) -> bool {
    let left = left
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>();
    let right = right
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>();
    !left.is_empty() && left.eq_ignore_ascii_case(&right)
}

fn unescape_markdown_text(value: &str) -> String {
    value.replace("\\\\", "\\")
}

fn preimage_part_to_request_value(part: &RichTextDto) -> AfsResult<Value> {
    let mut value = match part.kind.as_str() {
        "equation" => json!({
            "type": "equation",
            "equation": {
                "expression": part
                    .equation
                    .as_ref()
                    .map(|equation| equation.expression.as_str())
                    .unwrap_or(part.plain_text.as_str()),
            },
        }),
        "mention" => {
            let mention = part.mention.as_ref().ok_or_else(|| {
                AfsError::InvalidState(
                    "notion mention rich text had no mention payload".to_string(),
                )
            })?;
            json!({
                "type": "mention",
                "mention": mention_to_request_value(mention)?,
            })
        }
        _ => {
            let content = part
                .text
                .as_ref()
                .map(|text| text.content.as_str())
                .filter(|content| !content.is_empty())
                .unwrap_or(part.plain_text.as_str());
            let mut text = json!({ "content": content });
            if let Some(href) = rich_text_href(part) {
                text["link"] = json!({ "url": href });
            }
            json!({
                "type": "text",
                "text": text,
            })
        }
    };
    insert_annotations(&mut value, &InlineAnnotations::from(&part.annotations));
    Ok(value)
}

fn mention_to_request_value(mention: &crate::dto::MentionRichTextDto) -> AfsResult<Value> {
    match mention.kind.as_str() {
        "page" => Ok(json!({
            "type": "page",
            "page": {
                "id": mention
                    .page
                    .as_ref()
                    .map(|page| page.id.as_str())
                    .unwrap_or_default(),
            },
        })),
        "database" => Ok(json!({
            "type": "database",
            "database": {
                "id": mention
                    .database
                    .as_ref()
                    .map(|database| database.id.as_str())
                    .unwrap_or_default(),
            },
        })),
        "date" => {
            let date = mention.date.as_ref().ok_or_else(|| {
                AfsError::InvalidState("notion date mention had no date payload".to_string())
            })?;
            let mut value = json!({
                "type": "date",
                "date": {
                    "start": date.start,
                },
            });
            if let Some(end) = &date.end {
                value["date"]["end"] = json!(end);
            }
            if let Some(time_zone) = &date.time_zone {
                value["date"]["time_zone"] = json!(time_zone);
            }
            Ok(value)
        }
        "user" => Ok(json!({
            "type": "user",
            "user": {
                "id": mention
                    .user
                    .as_ref()
                    .map(|user| user.id.as_str())
                    .unwrap_or_default(),
            },
        })),
        _ => Err(AfsError::Unsupported("preserving this Notion mention type")),
    }
}

fn insert_annotations(value: &mut Value, annotations: &InlineAnnotations) {
    if annotations == &InlineAnnotations::default() {
        return;
    }

    value["annotations"] = json!({
        "bold": annotations.bold,
        "italic": annotations.italic,
        "strikethrough": annotations.strikethrough,
        "underline": annotations.underline,
        "code": annotations.code,
        "color": "default",
    });
}

fn render_rich_text_part_for_match(part: &RichTextDto) -> String {
    let (mut text, link_applied) = match part.kind.as_str() {
        "equation" => (equation_to_markdown(part), false),
        "mention" => mention_to_markdown(part),
        _ => (text_rich_text_to_markdown(part), false),
    };

    text = apply_annotations(text, &part.annotations);

    if !link_applied && let Some(href) = rich_text_href(part) {
        text = markdown_link_preserving_whitespace(&text, href);
    }

    text
}

fn text_rich_text_to_markdown(part: &RichTextDto) -> String {
    escape_markdown_text(&rich_text_part_plain_text(part))
}

fn equation_to_markdown(part: &RichTextDto) -> String {
    let expression = part
        .equation
        .as_ref()
        .map(|equation| equation.expression.as_str())
        .filter(|expression| !expression.is_empty())
        .unwrap_or(part.plain_text.as_str());

    if expression.is_empty() {
        String::new()
    } else {
        format!("${}$", expression.replace('$', "\\$"))
    }
}

fn mention_to_markdown(part: &RichTextDto) -> (String, bool) {
    let Some(mention) = &part.mention else {
        return (text_rich_text_to_markdown(part), false);
    };

    match mention.kind.as_str() {
        "page" => mention
            .page
            .as_ref()
            .map(|page| {
                (
                    markdown_link_preserving_whitespace(
                        &mention_label(part),
                        &notion_object_url(&page.id),
                    ),
                    true,
                )
            })
            .unwrap_or_else(|| (text_rich_text_to_markdown(part), false)),
        "database" => mention
            .database
            .as_ref()
            .map(|database| {
                (
                    markdown_link_preserving_whitespace(
                        &mention_label(part),
                        &notion_object_url(&database.id),
                    ),
                    true,
                )
            })
            .unwrap_or_else(|| (text_rich_text_to_markdown(part), false)),
        "date" => {
            let text = text_rich_text_to_markdown(part);
            if text.is_empty() {
                (
                    mention
                        .date
                        .as_ref()
                        .map(date_mention_label)
                        .map(|label| escape_markdown_text(&label))
                        .unwrap_or_default(),
                    false,
                )
            } else {
                (text, false)
            }
        }
        "user" => {
            let text = text_rich_text_to_markdown(part);
            if text.is_empty() {
                (
                    mention
                        .user
                        .as_ref()
                        .and_then(|user| user.name.clone())
                        .map(|name| escape_markdown_text(&format!("@{name}")))
                        .unwrap_or_default(),
                    false,
                )
            } else {
                (text, false)
            }
        }
        "link_preview" => mention
            .link_preview
            .as_ref()
            .filter(|link| !link.url.is_empty())
            .map(|link| {
                (
                    markdown_link_preserving_whitespace(&mention_label(part), &link.url),
                    true,
                )
            })
            .unwrap_or_else(|| (text_rich_text_to_markdown(part), false)),
        _ => (text_rich_text_to_markdown(part), false),
    }
}

fn mention_label(part: &RichTextDto) -> String {
    let label = rich_text_part_plain_text(part);
    if label.is_empty() {
        "mention".to_string()
    } else {
        escape_markdown_text(&label)
    }
}

fn date_mention_label(date: &crate::dto::DateMentionDto) -> String {
    match &date.end {
        Some(end) if !end.is_empty() => format!("{} to {end}", date.start),
        _ => date.start.clone(),
    }
}

fn apply_annotations(mut text: String, annotations: &RichTextAnnotationsDto) -> String {
    if annotations.code {
        text =
            wrap_preserving_whitespace(&text, |value| format!("`{}`", value.replace('`', "\\`")));
    }
    if annotations.bold {
        text = wrap_preserving_whitespace(&text, |value| format!("**{value}**"));
    }
    if annotations.italic {
        text = wrap_preserving_whitespace(&text, |value| format!("_{value}_"));
    }
    if annotations.strikethrough {
        text = wrap_preserving_whitespace(&text, |value| format!("~~{value}~~"));
    }
    if annotations.underline {
        text = wrap_preserving_whitespace(&text, |value| format!("<u>{value}</u>"));
    }

    text
}

fn rich_text_href(part: &RichTextDto) -> Option<&str> {
    part.href
        .as_deref()
        .or_else(|| {
            part.text
                .as_ref()?
                .link
                .as_ref()
                .map(|link| link.url.as_str())
        })
        .filter(|href| !href.is_empty())
}

fn rich_text_part_plain_text(part: &RichTextDto) -> String {
    if !part.plain_text.is_empty() {
        return part.plain_text.clone();
    }

    match part.kind.as_str() {
        "text" | "" => part
            .text
            .as_ref()
            .map(|text| text.content.clone())
            .unwrap_or_default(),
        "equation" => part
            .equation
            .as_ref()
            .map(|equation| equation.expression.clone())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn markdown_link_preserving_whitespace(label: &str, href: &str) -> String {
    wrap_preserving_whitespace(label, |value| {
        format!("[{}]({href})", escape_markdown_link_label(value))
    })
}

fn notion_object_url(id: &str) -> String {
    format!("https://www.notion.so/{}", notion_url_id(id))
}

fn notion_url_id(id: &str) -> String {
    let hex = id
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>();
    if hex.len() == 32 { hex } else { id.to_string() }
}

fn wrap_preserving_whitespace(value: &str, wrap: impl FnOnce(&str) -> String) -> String {
    let Some(start) = value
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(index, _)| index)
    else {
        return value.to_string();
    };
    let end = value
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(index, ch)| index + ch.len_utf8())
        .unwrap_or(value.len());

    format!(
        "{}{}{}",
        &value[..start],
        wrap(&value[start..end]),
        &value[end..]
    )
}

fn escape_markdown_text(text: &str) -> String {
    text.replace('\\', "\\\\")
}

fn escape_markdown_link_label(text: &str) -> String {
    text.replace('[', "\\[").replace(']', "\\]")
}

fn parse_code_fence(markdown: &str) -> Option<(String, String)> {
    let mut lines = markdown.lines();
    let first = lines.next()?.trim_start();
    let fence = if first.starts_with("```") {
        "```"
    } else if first.starts_with("~~~") {
        "~~~"
    } else {
        return None;
    };
    let language = first[fence.len()..].trim();
    let mut body = lines.collect::<Vec<_>>();
    if !body
        .last()
        .is_some_and(|line| line.trim_start().starts_with(fence))
    {
        return None;
    }
    body.pop();
    Some((language.to_string(), body.join("\n")))
}

fn parse_display_equation(markdown: &str) -> Option<String> {
    let trimmed = markdown.trim();
    if !trimmed.starts_with("$$") || !trimmed.ends_with("$$") || trimmed.len() < 4 {
        return None;
    }

    let expression = trimmed[2..trimmed.len() - 2].trim_matches('\n').trim();
    if expression.is_empty() {
        None
    } else {
        Some(expression.to_string())
    }
}

fn parse_heading(markdown: &str) -> Option<(usize, &str)> {
    let trimmed = markdown.trim_start();
    let level = trimmed.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) || !trimmed[level..].starts_with(' ') {
        return None;
    }

    Some((level, trimmed[level..].trim_start()))
}

fn parse_to_do(markdown: &str) -> Option<(bool, &str)> {
    let trimmed = markdown.trim_start();
    if let Some(text) = trimmed.strip_prefix("- [ ] ") {
        return Some((false, text));
    }
    if let Some(text) = trimmed
        .strip_prefix("- [x] ")
        .or_else(|| trimmed.strip_prefix("- [X] "))
    {
        return Some((true, text));
    }
    None
}

fn parse_bulleted_list_item(markdown: &str) -> Option<&str> {
    let trimmed = markdown.trim_start();
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
}

fn parse_numbered_list_item(markdown: &str) -> Option<&str> {
    let trimmed = markdown.trim_start();
    let digit_count = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_count == 0 || !trimmed[digit_count..].starts_with(". ") {
        return None;
    }

    Some(&trimmed[digit_count + 2..])
}

fn parse_quote(markdown: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        let text = trimmed.strip_prefix("> ")?;
        if text.starts_with("[!") {
            return None;
        }
        lines.push(text);
    }

    Some(lines.join("\n"))
}

fn parse_callout(markdown: &str) -> Option<String> {
    let mut lines = markdown.lines();
    let marker = lines.next()?.trim_start().strip_prefix("> ")?;
    if !marker.starts_with("[!") || !marker.ends_with(']') {
        return None;
    }

    let mut body = Vec::new();
    for line in lines {
        let trimmed = line.trim_start();
        let text = trimmed
            .strip_prefix("> ")
            .or_else(|| trimmed.strip_prefix('>'))?;
        body.push(text);
    }

    let text = body.join("\n");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn looks_like_markdown_table(markdown: &str) -> bool {
    let mut lines = markdown.lines();
    let Some(first) = lines.next() else {
        return false;
    };
    let Some(second) = lines.next() else {
        return false;
    };
    first.contains('|')
        && second.contains('|')
        && second
            .trim()
            .chars()
            .all(|ch| matches!(ch, '|' | '-' | ':' | ' '))
}

fn unsupported_undo_name(operation: &UndoOperation) -> &'static str {
    match operation {
        UndoOperation::MoveBlock { .. } => "undoing Notion block moves",
        UndoOperation::RestoreArchivedBlock { .. } => "restoring archived Notion blocks",
        UndoOperation::RestoreBlockContent { .. }
        | UndoOperation::ArchiveCreatedBlock { .. }
        | UndoOperation::ArchiveCreatedEntity { .. } => "unsupported Notion undo operation",
    }
}
