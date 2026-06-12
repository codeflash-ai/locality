//! Conflict data structures and block-overlap helpers.
//!
//! The core preserves local content when conflicts occur. Higher layers can
//! materialize inline conflict markers, but the collision decision is
//! deterministic and lives here.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::canonical::{parse_canonical_markdown, render_canonical_markdown};
use crate::model::CanonicalDocument;
use crate::model::RemoteId;

pub const CONFLICT_LOCAL_MARKER: &str = "<<<<<<< LOCAL";
pub const CONFLICT_SEPARATOR_MARKER: &str = "=======";
pub const CONFLICT_REMOTE_MARKER: &str = ">>>>>>> REMOTE";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictSummary {
    pub remote_id: RemoteId,
    pub path: PathBuf,
    pub remote_path: PathBuf,
    pub reason: ConflictReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictReason {
    LocalAndRemoteChanged,
    SameBlockChanged,
    RemoteMovedDuringPush,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConflictResolution {
    Ours,
    Theirs,
    Edited(PathBuf),
}

pub fn render_inline_conflict_markdown(
    local_contents: &str,
    remote_document: &CanonicalDocument,
) -> String {
    let (frontmatter, local_body) = match parse_canonical_markdown(local_contents) {
        Ok(parsed) => (parsed.document.frontmatter, parsed.document.body),
        Err(_) => (
            remote_document.frontmatter.clone(),
            local_contents.to_string(),
        ),
    };
    render_canonical_markdown(&CanonicalDocument::new(
        frontmatter,
        render_conflict_marker_body(&local_body, &remote_document.body),
    ))
}

pub fn render_conflict_marker_body(local_body: &str, remote_body: &str) -> String {
    let mut body = String::new();
    body.push_str(CONFLICT_LOCAL_MARKER);
    body.push('\n');
    push_marker_section(&mut body, local_body);
    body.push_str(CONFLICT_SEPARATOR_MARKER);
    body.push('\n');
    push_marker_section(&mut body, remote_body);
    body.push_str(CONFLICT_REMOTE_MARKER);
    body.push('\n');
    body
}

pub fn unresolved_conflict_marker_line(contents: &str) -> Option<usize> {
    let mut start_line = None;
    let mut saw_separator = false;

    for (index, line) in contents.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("<<<<<<<") {
            start_line = Some(index + 1);
            saw_separator = false;
        } else if start_line.is_some() && line == CONFLICT_SEPARATOR_MARKER {
            saw_separator = true;
        } else if start_line.is_some() && saw_separator && line.starts_with(">>>>>>>") {
            return start_line;
        }
    }

    None
}

pub fn has_unresolved_conflict_markers(contents: &str) -> bool {
    unresolved_conflict_marker_line(contents).is_some()
}

fn push_marker_section(output: &mut String, section: &str) {
    output.push_str(section);
    if !section.ends_with('\n') {
        output.push('\n');
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockChangeSet {
    changed_blocks: BTreeSet<RemoteId>,
    has_structural_change: bool,
}

impl BlockChangeSet {
    pub fn from_blocks(blocks: impl IntoIterator<Item = RemoteId>) -> Self {
        Self {
            changed_blocks: blocks.into_iter().collect(),
            has_structural_change: false,
        }
    }

    pub fn structural() -> Self {
        Self {
            changed_blocks: BTreeSet::new(),
            has_structural_change: true,
        }
    }

    pub fn with_structural_change(mut self) -> Self {
        self.has_structural_change = true;
        self
    }

    pub fn is_disjoint(&self, other: &Self) -> bool {
        !self.has_structural_change
            && !other.has_structural_change
            && self.changed_blocks.is_disjoint(&other.changed_blocks)
    }

    pub fn len(&self) -> usize {
        self.changed_blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.changed_blocks.is_empty() && !self.has_structural_change
    }
}
