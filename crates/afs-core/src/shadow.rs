//! Shadow snapshots and Markdown block segmentation.
//!
//! The state store will persist shadow snapshots as the last rendered text both
//! sides agreed on. This module keeps the connector-neutral part: stable block
//! boundaries, source spans, block content hashes, and remote block IDs.
//!
//! The segmentation here is intentionally conservative. It recognizes common
//! Markdown shapes well enough for a first deterministic push planner. More
//! sophisticated alignment can replace the internals without changing the
//! snapshot contract.

use std::collections::BTreeSet;
use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::canonical::parse_directive_line;
use crate::model::{RemoteId, SourceSpan};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowDocument {
    pub entity_id: RemoteId,
    pub body_hash: String,
    pub rendered_body: String,
    pub blocks: Vec<ShadowBlock>,
}

impl ShadowDocument {
    pub fn from_synced_body(
        entity_id: RemoteId,
        body: impl Into<String>,
        body_start_line: usize,
        native_block_ids: impl IntoIterator<Item = RemoteId>,
    ) -> Result<Self, ShadowBuildError> {
        let rendered_body = body.into();
        let segmented_blocks = segment_markdown_body(&rendered_body, body_start_line);
        let mut native_block_ids = native_block_ids.into_iter();
        let mut blocks = Vec::with_capacity(segmented_blocks.len());

        for block in segmented_blocks {
            let remote_id = match block.remote_id.clone() {
                Some(remote_id) => remote_id,
                None if block.kind.is_directive() => {
                    return Err(ShadowBuildError::MalformedDirective {
                        line: block.source_span.start_line,
                    });
                }
                None => native_block_ids.next().ok_or({
                    ShadowBuildError::MissingNativeBlockId {
                        line: block.source_span.start_line,
                    }
                })?,
            };

            blocks.push(ShadowBlock {
                remote_id,
                kind: block.kind,
                source_span: block.source_span,
                content_hash: block.content_hash,
                text: block.text,
            });
        }

        if let Some(extra_id) = native_block_ids.next() {
            return Err(ShadowBuildError::UnusedNativeBlockId {
                remote_id: extra_id,
            });
        }

        Ok(Self {
            entity_id,
            body_hash: stable_hash(&rendered_body),
            rendered_body,
            blocks,
        })
    }

    pub fn block_ids(&self) -> BTreeSet<RemoteId> {
        self.blocks
            .iter()
            .map(|block| block.remote_id.clone())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowBlock {
    pub remote_id: RemoteId,
    pub kind: MarkdownBlockKind,
    pub source_span: SourceSpan,
    pub content_hash: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentedBlock {
    pub remote_id: Option<RemoteId>,
    pub kind: MarkdownBlockKind,
    pub source_span: SourceSpan,
    pub content_hash: String,
    pub text: String,
}

impl SegmentedBlock {
    pub fn is_directive(&self) -> bool {
        matches!(self.kind, MarkdownBlockKind::Directive { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MarkdownBlockKind {
    Heading,
    Paragraph,
    List,
    CodeFence,
    Table,
    TableWithRows {
        row_ids: Vec<RemoteId>,
        has_column_header: bool,
        has_row_header: bool,
    },
    Directive {
        directive_type: Option<String>,
        raw: String,
        malformed: bool,
    },
}

impl MarkdownBlockKind {
    pub fn is_directive(&self) -> bool {
        matches!(self, Self::Directive { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShadowBuildError {
    MissingNativeBlockId { line: usize },
    MalformedDirective { line: usize },
    UnusedNativeBlockId { remote_id: RemoteId },
}

impl Display for ShadowBuildError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingNativeBlockId { line } => {
                write!(
                    f,
                    "missing native block id for block starting on line {line}"
                )
            }
            Self::MalformedDirective { line } => {
                write!(f, "malformed directive in shadow on line {line}")
            }
            Self::UnusedNativeBlockId { remote_id } => {
                write!(f, "unused native block id `{}`", remote_id.0)
            }
        }
    }
}

impl std::error::Error for ShadowBuildError {}

pub fn segment_markdown_body(body: &str, body_start_line: usize) -> Vec<SegmentedBlock> {
    let lines: Vec<&str> = body.lines().collect();
    let mut blocks = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        if lines[index].trim().is_empty() {
            index += 1;
            continue;
        }

        let end = block_end(&lines, index);
        let text = lines[index..end].join("\n");
        let kind = classify_block(&lines, index, end);
        let remote_id = match &kind {
            MarkdownBlockKind::Directive { .. } => {
                parse_directive_line(lines[index], body_start_line + index)
                    .and_then(|directive| directive.remote_id)
            }
            _ => None,
        };
        let source_span = SourceSpan {
            start_line: body_start_line + index,
            end_line: body_start_line + end - 1,
        };

        blocks.push(SegmentedBlock {
            remote_id,
            content_hash: block_hash(&kind, &text),
            kind,
            source_span,
            text,
        });
        index = end;
    }

    blocks
}

pub fn stable_hash(input: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    format!("{hash:016x}")
}

fn block_hash(kind: &MarkdownBlockKind, text: &str) -> String {
    stable_hash(&format!("{}:{text}", kind_tag(kind)))
}

fn block_end(lines: &[&str], start: usize) -> usize {
    if parse_directive_line(lines[start], start + 1).is_some() || is_heading(lines[start]) {
        return start + 1;
    }

    if let Some(fence) = fence_marker(lines[start]) {
        return consume_code_fence(lines, start, fence);
    }

    if is_table_start(lines, start) {
        return consume_while(lines, start, |line| {
            line.contains('|') && !line.trim().is_empty()
        });
    }

    if is_list_item(lines[start]) {
        return consume_while(lines, start, |line| {
            !line.trim().is_empty()
                && (is_list_item(line) || line.starts_with(' ') || line.starts_with('\t'))
        });
    }

    consume_while(lines, start, |line| {
        !line.trim().is_empty()
            && parse_directive_line(line, 1).is_none()
            && !is_heading(line)
            && fence_marker(line).is_none()
            && !is_table_start(lines, start)
            && !is_list_item(line)
    })
}

fn classify_block(lines: &[&str], start: usize, end: usize) -> MarkdownBlockKind {
    let first = lines[start];

    if let Some(directive) = parse_directive_line(first, start + 1) {
        return MarkdownBlockKind::Directive {
            directive_type: directive.directive_type,
            raw: directive.raw,
            malformed: directive.malformed,
        };
    }

    if is_heading(first) {
        MarkdownBlockKind::Heading
    } else if fence_marker(first).is_some() {
        MarkdownBlockKind::CodeFence
    } else if is_table_start(lines, start) {
        MarkdownBlockKind::Table
    } else if end > start && is_list_item(first) {
        MarkdownBlockKind::List
    } else {
        MarkdownBlockKind::Paragraph
    }
}

fn consume_code_fence(lines: &[&str], start: usize, fence: FenceMarker) -> usize {
    for (offset, line) in lines[start + 1..].iter().enumerate() {
        if line.trim_start().starts_with(fence.marker) {
            return start + offset + 2;
        }
    }

    lines.len()
}

fn consume_while(lines: &[&str], start: usize, predicate: impl Fn(&str) -> bool) -> usize {
    let mut end = start;

    while end < lines.len() && predicate(lines[end]) {
        end += 1;
    }

    end
}

fn is_heading(line: &str) -> bool {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|ch| *ch == '#').count();
    (1..=6).contains(&level) && trimmed.chars().nth(level).is_some_and(char::is_whitespace)
}

fn is_list_item(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("+ ")
        || trimmed.starts_with("- [ ] ")
        || trimmed.starts_with("- [x] ")
        || ordered_list_marker(trimmed)
}

fn ordered_list_marker(line: &str) -> bool {
    let digit_count = line.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digit_count > 0 && line[digit_count..].starts_with(". ")
}

fn is_table_start(lines: &[&str], start: usize) -> bool {
    start + 1 < lines.len() && lines[start].contains('|') && is_table_separator(lines[start + 1])
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains('|')
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '|' | '-' | ':' | ' '))
        && trimmed.contains('-')
}

fn fence_marker(line: &str) -> Option<FenceMarker> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some(FenceMarker { marker: "```" })
    } else if trimmed.starts_with("~~~") {
        Some(FenceMarker { marker: "~~~" })
    } else {
        None
    }
}

fn kind_tag(kind: &MarkdownBlockKind) -> &'static str {
    match kind {
        MarkdownBlockKind::Heading => "heading",
        MarkdownBlockKind::Paragraph => "paragraph",
        MarkdownBlockKind::List => "list",
        MarkdownBlockKind::CodeFence => "code",
        MarkdownBlockKind::Table | MarkdownBlockKind::TableWithRows { .. } => "table",
        MarkdownBlockKind::Directive { .. } => "directive",
    }
}

#[derive(Clone, Copy)]
struct FenceMarker {
    marker: &'static str,
}
