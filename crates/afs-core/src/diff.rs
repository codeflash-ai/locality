//! Block-aware diff engine interface.
//!
//! The real engine will align rendered text against shadow block snapshots in
//! exact, structural, and residual passes. The current trait lets higher layers
//! depend on that boundary while the implementation is built incrementally.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::model::{CanonicalDocument, RemoteId};
use crate::planner::{PlanDegradation, PlanDegradationKind, PushOperation, PushPlan};
use crate::shadow::{
    MarkdownBlockKind, SegmentedBlock, ShadowBlock, ShadowDocument, segment_markdown_body,
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
    let mut operations = Vec::new();
    let mut matched_shadow = BTreeSet::new();
    let mut previous_existing_id: Option<RemoteId> = None;

    for (edited_index, edited_block) in edited_blocks.iter().enumerate() {
        match matches[edited_index] {
            Some(shadow_index) => {
                matched_shadow.insert(shadow_index);
                let shadow_block = &shadow.blocks[shadow_index];

                if should_move_block(shadow_index, edited_index, shadow_block, edited_block) {
                    operations.push(PushOperation::MoveBlock {
                        block_id: shadow_block.remote_id.clone(),
                        after: previous_existing_id.clone(),
                    });
                }

                if shadow_block.content_hash != edited_block.content_hash {
                    operations.push(PushOperation::UpdateBlock {
                        block_id: shadow_block.remote_id.clone(),
                        content: edited_block.text.clone(),
                    });
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
        if !matched_shadow.contains(&index) {
            operations.push(PushOperation::ArchiveBlock {
                block_id: shadow_block.remote_id.clone(),
            });
        }
    }

    Ok(PushPlan::new(vec![shadow.entity_id.clone()], operations).with_degradations(degradations))
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

    if residual_edited.len() > 1 && residual_shadow.len() > 1 {
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

fn should_move_block(
    shadow_index: usize,
    edited_index: usize,
    shadow_block: &ShadowBlock,
    edited_block: &SegmentedBlock,
) -> bool {
    shadow_index != edited_index
        && shadow_block.kind.is_directive()
        && edited_block.is_directive()
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
