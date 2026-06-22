//! Block-aware diff engine interface.
//!
//! The real engine will align rendered text against shadow block snapshots in
//! exact, structural, and residual passes. The current trait lets higher layers
//! depend on that boundary while the implementation is built incrementally.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use yaml_serde::Value;

use crate::canonical::{parse_canonical_markdown, render_canonical_markdown};
use crate::model::{CanonicalDocument, RemoteId};
use crate::planner::{
    PlanDegradation, PlanDegradationKind, PropertyValue, PushOperation, PushPlan,
};
use crate::shadow::{
    MarkdownBlockKind, SegmentedBlock, ShadowBlock, ShadowDocument, rendered_bodies_equivalent,
    segment_markdown_body,
};
use crate::validation::{ValidationIssue, ValidationReport};
use crate::{AfsError, AfsResult};

pub trait DiffEngine {
    fn plan_push(&self, shadow: &ShadowDocument, edited: &CanonicalDocument)
    -> AfsResult<PushPlan>;
}

#[derive(Clone, Debug, Default)]
pub struct StubDiffEngine;

impl DiffEngine for StubDiffEngine {
    fn plan_push(
        &self,
        _shadow: &ShadowDocument,
        _edited: &CanonicalDocument,
    ) -> AfsResult<PushPlan> {
        Err(AfsError::NotImplemented("block-aware diff engine"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlignmentPass {
    Exact,
    Structural,
    Residual,
}

#[derive(Clone, Debug, Default)]
pub struct BlockDiffEngine {
    pub edited_body_start_line: usize,
}

impl BlockDiffEngine {
    pub fn new() -> Self {
        Self {
            edited_body_start_line: 1,
        }
    }

    pub fn with_edited_body_start_line(mut self, line: usize) -> Self {
        self.edited_body_start_line = line;
        self
    }
}

impl DiffEngine for BlockDiffEngine {
    fn plan_push(
        &self,
        shadow: &ShadowDocument,
        edited: &CanonicalDocument,
    ) -> AfsResult<PushPlan> {
        plan_block_diff(shadow, edited, self.edited_body_start_line)
    }
}

pub fn plan_block_diff(
    shadow: &ShadowDocument,
    edited: &CanonicalDocument,
    edited_body_start_line: usize,
) -> AfsResult<PushPlan> {
    let edited_blocks = segment_markdown_body(&edited.body, edited_body_start_line);
    let validation = validate_edited_directives(shadow, &edited_blocks);
    if !validation.is_clean() {
        return Err(AfsError::Validation(validation.issues));
    }

    let (matches, degradations) = align_blocks(shadow, &edited_blocks);
    let mut operations = property_diff_operations(shadow, edited)?;
    let retained_shadow = matches
        .iter()
        .filter_map(|shadow_index| *shadow_index)
        .collect::<BTreeSet<_>>();
    let mut recreated_shadow = BTreeSet::new();
    let mut moved_existing_ids = BTreeSet::new();
    let mut previous_existing_id: Option<RemoteId> = None;

    for (edited_index, edited_block) in edited_blocks.iter().enumerate() {
        match matches[edited_index] {
            Some(shadow_index) => {
                let shadow_block = &shadow.blocks[shadow_index];
                let previous_retained_id = previous_retained_shadow_id(
                    shadow,
                    shadow_index,
                    &retained_shadow,
                    &recreated_shadow,
                );

                if should_move_block(
                    shadow_block,
                    edited_block,
                    previous_retained_id,
                    previous_existing_id.as_ref(),
                ) {
                    operations.push(PushOperation::MoveBlock {
                        block_id: shadow_block.remote_id.clone(),
                        after: previous_existing_id.clone(),
                    });
                    moved_existing_ids.insert(shadow_block.remote_id.clone());
                }

                if should_recreate_moved_native_block(
                    shadow_block,
                    edited_block,
                    previous_retained_id,
                    previous_existing_id.as_ref(),
                    &moved_existing_ids,
                ) {
                    operations.push(PushOperation::AppendBlock {
                        parent_id: shadow.entity_id.clone(),
                        after: previous_existing_id.clone(),
                        content: edited_block.text.clone(),
                    });
                    recreated_shadow.insert(shadow_index);
                    continue;
                }

                let write_kind_changed = should_replace_block(shadow_block, edited_block);
                if write_kind_changed
                    || !rendered_bodies_equivalent(&shadow_block.text, &edited_block.text)
                {
                    if write_kind_changed {
                        operations.push(PushOperation::ReplaceBlock {
                            block_id: shadow_block.remote_id.clone(),
                            content: edited_block.text.clone(),
                        });
                    } else {
                        operations.push(PushOperation::UpdateBlock {
                            block_id: shadow_block.remote_id.clone(),
                            content: edited_block.text.clone(),
                        });
                    }
                }

                previous_existing_id = Some(shadow_block.remote_id.clone());
            }
            None => {
                operations.push(PushOperation::AppendBlock {
                    parent_id: shadow.entity_id.clone(),
                    after: previous_existing_id.clone(),
                    content: edited_block.text.clone(),
                });
            }
        }
    }

    for (index, shadow_block) in shadow.blocks.iter().enumerate() {
        if !retained_shadow.contains(&index) || recreated_shadow.contains(&index) {
            operations.push(PushOperation::ArchiveBlock {
                block_id: shadow_block.remote_id.clone(),
            });
        }
    }

    Ok(PushPlan::new(vec![shadow.entity_id.clone()], operations).with_degradations(degradations))
}

fn property_diff_operations(
    shadow: &ShadowDocument,
    edited: &CanonicalDocument,
) -> AfsResult<Vec<PushOperation>> {
    if shadow.frontmatter.trim().is_empty() {
        return Ok(Vec::new());
    }

    let synced = parse_canonical_markdown(&render_canonical_markdown(&CanonicalDocument::new(
        shadow.frontmatter.clone(),
        shadow.rendered_body.clone(),
    )))
    .map_err(|error| {
        AfsError::InvalidState(format!(
            "synced shadow frontmatter is no longer parseable: {error}"
        ))
    })?;
    let edited = parse_canonical_markdown(&render_canonical_markdown(edited)).map_err(|error| {
        AfsError::InvalidState(format!(
            "edited frontmatter is no longer parseable: {error}"
        ))
    })?;

    let mut updates = BTreeMap::new();
    if synced.frontmatter.title != edited.frontmatter.title {
        updates.insert(
            "title".to_string(),
            edited
                .frontmatter
                .title
                .map(PropertyValue::String)
                .unwrap_or(PropertyValue::Null),
        );
    }

    let keys = synced
        .frontmatter
        .properties
        .keys()
        .chain(edited.frontmatter.properties.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in keys {
        let synced_value = synced.frontmatter.properties.get(&key);
        let edited_value = edited.frontmatter.properties.get(&key);
        if synced_value != edited_value {
            updates.insert(
                key.clone(),
                edited_value
                    .map(property_value_from_frontmatter)
                    .unwrap_or(PropertyValue::Null),
            );
        }
    }

    if updates.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(vec![PushOperation::UpdateProperties {
            entity_id: shadow.entity_id.clone(),
            properties: updates,
        }])
    }
}

pub fn property_value_from_frontmatter(value: &Value) -> PropertyValue {
    match value {
        Value::Null => PropertyValue::Null,
        Value::Bool(value) => PropertyValue::Bool(*value),
        Value::Number(value) => PropertyValue::Number(value.to_string()),
        Value::String(value) => PropertyValue::String(value.clone()),
        Value::Sequence(values) => PropertyValue::List(
            values
                .iter()
                .filter_map(simple_frontmatter_string)
                .collect::<Vec<_>>(),
        ),
        Value::Mapping(mapping) => PropertyValue::Object(
            mapping
                .iter()
                .filter_map(|(key, value)| {
                    simple_frontmatter_string(key)
                        .map(|key| (key, property_value_from_frontmatter(value)))
                })
                .collect(),
        ),
        Value::Tagged(tagged) => property_value_from_frontmatter(&tagged.value),
    }
}

fn simple_frontmatter_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn validate_edited_directives(
    shadow: &ShadowDocument,
    edited_blocks: &[SegmentedBlock],
) -> ValidationReport {
    let mut report = ValidationReport::clean();
    let shadow_directives = shadow_directives_by_id(shadow);

    for block in edited_blocks
        .iter()
        .filter(|block| matches!(block.kind, MarkdownBlockKind::Directive { .. }))
    {
        let MarkdownBlockKind::Directive {
            directive_type,
            raw,
            malformed,
        } = &block.kind
        else {
            continue;
        };

        if *malformed {
            report.push(issue(
                "directive_malformed",
                block.source_span.start_line,
                "AgentFS directive syntax is malformed",
            ));
            continue;
        }

        let Some(remote_id) = block.remote_id.as_ref() else {
            report.push(issue(
                "directive_missing_id",
                block.source_span.start_line,
                "AgentFS directive is missing an `id` attribute",
            ));
            continue;
        };

        if directive_type.is_none() {
            report.push(issue(
                "directive_missing_type",
                block.source_span.start_line,
                "AgentFS directive is missing a `type` attribute",
            ));
            continue;
        }

        let Some(shadow_block) = shadow_directives.get(remote_id) else {
            report.push(issue(
                "directive_unknown",
                block.source_span.start_line,
                format!(
                    "directive anchor `{}` was not present in the synced shadow",
                    remote_id.0
                ),
            ));
            continue;
        };

        let MarkdownBlockKind::Directive {
            directive_type: shadow_type,
            raw: shadow_raw,
            ..
        } = &shadow_block.kind
        else {
            continue;
        };

        if directive_type != shadow_type || raw != shadow_raw {
            report.push(issue(
                "directive_mangled",
                block.source_span.start_line,
                format!("directive anchor `{}` was edited", remote_id.0),
            ));
        }
    }

    report
}

fn align_blocks(
    shadow: &ShadowDocument,
    edited_blocks: &[SegmentedBlock],
) -> (Vec<Option<usize>>, Vec<PlanDegradation>) {
    let mut matches = vec![None; edited_blocks.len()];
    let mut used_shadow = BTreeSet::new();

    align_directives(shadow, edited_blocks, &mut matches, &mut used_shadow);
    align_exact_hashes(shadow, edited_blocks, &mut matches, &mut used_shadow);
    let degradation =
        align_residual_by_order(shadow, edited_blocks, &mut matches, &mut used_shadow);

    (matches, degradation.into_iter().collect())
}

fn align_directives(
    shadow: &ShadowDocument,
    edited_blocks: &[SegmentedBlock],
    matches: &mut [Option<usize>],
    used_shadow: &mut BTreeSet<usize>,
) {
    let shadow_directives = shadow_directive_indexes_by_id(shadow);

    for (edited_index, block) in edited_blocks.iter().enumerate() {
        if !block.is_directive() {
            continue;
        }

        let Some(remote_id) = block.remote_id.as_ref() else {
            continue;
        };

        if let Some(shadow_index) = shadow_directives.get(remote_id)
            && used_shadow.insert(*shadow_index)
        {
            matches[edited_index] = Some(*shadow_index);
        }
    }
}

fn align_exact_hashes(
    shadow: &ShadowDocument,
    edited_blocks: &[SegmentedBlock],
    matches: &mut [Option<usize>],
    used_shadow: &mut BTreeSet<usize>,
) {
    let hash_index = unique_native_shadow_hashes(shadow, used_shadow);

    for (edited_index, block) in edited_blocks.iter().enumerate() {
        if matches[edited_index].is_some() || block.is_directive() {
            continue;
        }

        if let Some(shadow_index) = hash_index.get(&block.content_hash)
            && used_shadow.insert(*shadow_index)
        {
            matches[edited_index] = Some(*shadow_index);
        }
    }
}

fn align_residual_by_order(
    shadow: &ShadowDocument,
    edited_blocks: &[SegmentedBlock],
    matches: &mut [Option<usize>],
    used_shadow: &mut BTreeSet<usize>,
) -> Option<PlanDegradation> {
    let residual_edited: Vec<_> = edited_blocks
        .iter()
        .enumerate()
        .filter(|(index, block)| matches[*index].is_none() && !block.is_directive())
        .map(|(index, _)| index)
        .collect();
    let residual_shadow: Vec<_> = shadow
        .blocks
        .iter()
        .enumerate()
        .filter(|(index, block)| !used_shadow.contains(index) && !block.kind.is_directive())
        .map(|(index, _)| index)
        .collect();

    if residual_edited.len() > 1
        && residual_shadow.len() > 1
        && !residual_kinds_match_common_prefix(
            shadow,
            edited_blocks,
            &residual_shadow,
            &residual_edited,
        )
    {
        return Some(PlanDegradation::new(
            PlanDegradationKind::AmbiguousBlockAlignment,
            "multiple edited and synced blocks could not be aligned safely; unmatched edited blocks will be appended and unmatched synced blocks archived",
        ));
    }

    for (edited_index, shadow_index) in residual_edited.iter().zip(residual_shadow.iter()) {
        matches[*edited_index] = Some(*shadow_index);
        used_shadow.insert(*shadow_index);
    }

    None
}

fn residual_kinds_match_common_prefix(
    shadow: &ShadowDocument,
    edited_blocks: &[SegmentedBlock],
    residual_shadow: &[usize],
    residual_edited: &[usize],
) -> bool {
    residual_shadow
        .iter()
        .zip(residual_edited)
        .all(|(shadow_index, edited_index)| {
            same_alignment_kind(
                &shadow.blocks[*shadow_index].kind,
                &edited_blocks[*edited_index].kind,
            )
        })
}

fn same_alignment_kind(left: &MarkdownBlockKind, right: &MarkdownBlockKind) -> bool {
    match (left, right) {
        (
            MarkdownBlockKind::TableWithRows {
                has_column_header: left_column,
                has_row_header: left_row,
                ..
            },
            MarkdownBlockKind::TableWithRows {
                has_column_header: right_column,
                has_row_header: right_row,
                ..
            },
        ) => left_column == right_column && left_row == right_row,
        (MarkdownBlockKind::TableWithRows { .. }, MarkdownBlockKind::Table)
        | (MarkdownBlockKind::Table, MarkdownBlockKind::TableWithRows { .. }) => true,
        (left, right) => left == right,
    }
}

fn should_replace_block(shadow_block: &ShadowBlock, edited_block: &SegmentedBlock) -> bool {
    markdown_write_kind(&shadow_block.kind, &shadow_block.text)
        != markdown_write_kind(&edited_block.kind, &edited_block.text)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkdownWriteKind {
    Heading(usize),
    Paragraph,
    Quote,
    Callout,
    BulletedList,
    NumberedList,
    ToDo,
    Code,
    Table,
    Divider,
    Equation,
    Directive,
}

fn markdown_write_kind(kind: &MarkdownBlockKind, text: &str) -> MarkdownWriteKind {
    let trimmed = text.trim_end_matches('\n');
    match kind {
        MarkdownBlockKind::Heading => parse_heading_level(trimmed)
            .map(MarkdownWriteKind::Heading)
            .unwrap_or(MarkdownWriteKind::Heading(0)),
        MarkdownBlockKind::Paragraph => paragraph_write_kind(trimmed),
        MarkdownBlockKind::List => list_write_kind(trimmed),
        MarkdownBlockKind::CodeFence => MarkdownWriteKind::Code,
        MarkdownBlockKind::Table | MarkdownBlockKind::TableWithRows { .. } => {
            MarkdownWriteKind::Table
        }
        MarkdownBlockKind::Directive { .. } => MarkdownWriteKind::Directive,
    }
}

fn paragraph_write_kind(markdown: &str) -> MarkdownWriteKind {
    let trimmed = markdown.trim();
    if trimmed == "---" {
        return MarkdownWriteKind::Divider;
    }
    if is_display_equation(trimmed) {
        return MarkdownWriteKind::Equation;
    }
    if is_callout(markdown) {
        return MarkdownWriteKind::Callout;
    }
    if is_quote(markdown) {
        return MarkdownWriteKind::Quote;
    }

    MarkdownWriteKind::Paragraph
}

fn list_write_kind(markdown: &str) -> MarkdownWriteKind {
    let trimmed = markdown.trim_start();
    if is_to_do_item(trimmed) {
        MarkdownWriteKind::ToDo
    } else if is_numbered_item(trimmed) {
        MarkdownWriteKind::NumberedList
    } else {
        MarkdownWriteKind::BulletedList
    }
}

fn parse_heading_level(markdown: &str) -> Option<usize> {
    let trimmed = markdown.trim_start();
    let level = trimmed.chars().take_while(|ch| *ch == '#').count();
    if (1..=6).contains(&level) && trimmed[level..].starts_with(' ') {
        Some(level)
    } else {
        None
    }
}

fn is_to_do_item(trimmed: &str) -> bool {
    trimmed.starts_with("- [ ] ")
        || trimmed.starts_with("- [] ")
        || trimmed.starts_with("- [x] ")
        || trimmed.starts_with("- [X] ")
}

fn is_numbered_item(trimmed: &str) -> bool {
    let digit_count = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digit_count > 0 && trimmed[digit_count..].starts_with(". ")
}

fn is_display_equation(trimmed: &str) -> bool {
    trimmed.starts_with("$$") && trimmed.ends_with("$$") && trimmed.len() >= 4
}

fn is_callout(markdown: &str) -> bool {
    let mut lines = markdown.lines();
    let Some(marker) = lines.next().map(str::trim_start) else {
        return false;
    };
    marker
        .strip_prefix("> ")
        .is_some_and(|marker| marker.starts_with("[!") && marker.ends_with(']'))
}

fn is_quote(markdown: &str) -> bool {
    let mut saw_line = false;
    for line in markdown.lines() {
        saw_line = true;
        let Some(text) = line.trim_start().strip_prefix("> ") else {
            return false;
        };
        if text.starts_with("[!") {
            return false;
        }
    }
    saw_line
}

fn previous_retained_shadow_id<'a>(
    shadow: &'a ShadowDocument,
    shadow_index: usize,
    retained_shadow: &BTreeSet<usize>,
    recreated_shadow: &BTreeSet<usize>,
) -> Option<&'a RemoteId> {
    (0..shadow_index)
        .rev()
        .find(|index| retained_shadow.contains(index) && !recreated_shadow.contains(index))
        .map(|index| &shadow.blocks[index].remote_id)
}

fn should_move_block(
    shadow_block: &ShadowBlock,
    edited_block: &SegmentedBlock,
    previous_retained_id: Option<&RemoteId>,
    previous_existing_id: Option<&RemoteId>,
) -> bool {
    previous_retained_id != previous_existing_id
        && shadow_block.kind.is_directive()
        && edited_block.is_directive()
        && shadow_block.content_hash == edited_block.content_hash
}

fn should_recreate_moved_native_block(
    shadow_block: &ShadowBlock,
    edited_block: &SegmentedBlock,
    previous_retained_id: Option<&RemoteId>,
    previous_existing_id: Option<&RemoteId>,
    moved_existing_ids: &BTreeSet<RemoteId>,
) -> bool {
    previous_retained_id != previous_existing_id
        && !previous_existing_id.is_some_and(|remote_id| moved_existing_ids.contains(remote_id))
        && !shadow_block.kind.is_directive()
        && !edited_block.is_directive()
        && shadow_block.content_hash == edited_block.content_hash
}

fn shadow_directives_by_id(shadow: &ShadowDocument) -> BTreeMap<&RemoteId, &ShadowBlock> {
    shadow
        .blocks
        .iter()
        .filter(|block| block.kind.is_directive())
        .map(|block| (&block.remote_id, block))
        .collect()
}

fn shadow_directive_indexes_by_id(shadow: &ShadowDocument) -> BTreeMap<&RemoteId, usize> {
    shadow
        .blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| block.kind.is_directive())
        .map(|(index, block)| (&block.remote_id, index))
        .collect()
}

fn unique_native_shadow_hashes(
    shadow: &ShadowDocument,
    used_shadow: &BTreeSet<usize>,
) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (index, block) in shadow.blocks.iter().enumerate() {
        if used_shadow.contains(&index) || block.kind.is_directive() {
            continue;
        }
        *counts.entry(block.content_hash.clone()).or_default() += 1;
    }

    shadow
        .blocks
        .iter()
        .enumerate()
        .filter(|(index, block)| {
            !used_shadow.contains(index)
                && !block.kind.is_directive()
                && counts.get(&block.content_hash) == Some(&1)
        })
        .map(|(index, block)| (block.content_hash.clone(), index))
        .collect()
}

fn issue(code: impl Into<String>, line: usize, message: impl Into<String>) -> ValidationIssue {
    ValidationIssue::new(
        code,
        PathBuf::new(),
        Some(line),
        message,
        Some("restore the directive line exactly or delete it to delete the block".to_string()),
    )
}
