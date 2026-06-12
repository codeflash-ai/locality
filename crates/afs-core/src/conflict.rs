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
    render_inline_conflict_markdown_with_base(local_contents, None, remote_document)
}

pub fn render_inline_conflict_markdown_with_base(
    local_contents: &str,
    base_body: Option<&str>,
    remote_document: &CanonicalDocument,
) -> String {
    let (frontmatter, local_body) = match parse_canonical_markdown(local_contents) {
        Ok(parsed) => (parsed.document.frontmatter, parsed.document.body),
        Err(_) => (
            remote_document.frontmatter.clone(),
            local_contents.to_string(),
        ),
    };
    let body = match base_body {
        Some(base_body) => {
            render_conflict_marker_body_with_base(base_body, &local_body, &remote_document.body)
        }
        None => render_conflict_marker_body(&local_body, &remote_document.body),
    };

    render_canonical_markdown(&CanonicalDocument::new(frontmatter, body))
}

pub fn render_conflict_marker_body(local_body: &str, remote_body: &str) -> String {
    render_conflict_hunk(local_body, remote_body)
}

pub fn render_conflict_marker_body_with_base(
    base_body: &str,
    local_body: &str,
    remote_body: &str,
) -> String {
    let Some(local_changes) = line_changes(base_body, local_body) else {
        return render_conflict_marker_body(local_body, remote_body);
    };
    let Some(remote_changes) = line_changes(base_body, remote_body) else {
        return render_conflict_marker_body(local_body, remote_body);
    };

    merge_line_changes(base_body, &local_changes, &remote_changes)
}

fn merge_line_changes(
    base_body: &str,
    local_changes: &[LineChange],
    remote_changes: &[LineChange],
) -> String {
    let base_lines = split_lines(base_body);
    let mut body = String::new();
    let mut base_cursor = 0;
    let mut local_index = 0;
    let mut remote_index = 0;

    while local_index < local_changes.len() || remote_index < remote_changes.len() {
        let next_start = match (
            local_changes.get(local_index),
            remote_changes.get(remote_index),
        ) {
            (Some(local), Some(remote)) => local.base_start.min(remote.base_start),
            (Some(local), None) => local.base_start,
            (None, Some(remote)) => remote.base_start,
            (None, None) => break,
        };

        push_lines(&mut body, &base_lines[base_cursor..next_start]);

        let group_start = next_start;
        let mut group_end = next_start;
        let local_start = local_index;
        let remote_start = remote_index;

        loop {
            let mut changed = false;

            while let Some(change) = local_changes.get(local_index) {
                if change.base_start > group_end {
                    break;
                }
                group_end = group_end.max(change.base_end);
                local_index += 1;
                changed = true;
            }

            while let Some(change) = remote_changes.get(remote_index) {
                if change.base_start > group_end {
                    break;
                }
                group_end = group_end.max(change.base_end);
                remote_index += 1;
                changed = true;
            }

            if !changed {
                break;
            }
        }

        let local_group = &local_changes[local_start..local_index];
        let remote_group = &remote_changes[remote_start..remote_index];
        let local_region = render_changed_region(&base_lines, group_start, group_end, local_group);
        let remote_region =
            render_changed_region(&base_lines, group_start, group_end, remote_group);

        match (local_group.is_empty(), remote_group.is_empty()) {
            (false, true) => body.push_str(&local_region),
            (true, false) => body.push_str(&remote_region),
            (false, false) if local_region == remote_region => body.push_str(&local_region),
            (false, false) => body.push_str(&render_conflict_hunk(&local_region, &remote_region)),
            (true, true) => {}
        }

        base_cursor = group_end;
    }

    push_lines(&mut body, &base_lines[base_cursor..]);
    body
}

fn render_changed_region(
    base_lines: &[String],
    start: usize,
    end: usize,
    changes: &[LineChange],
) -> String {
    let mut region = String::new();
    let mut cursor = start;

    for change in changes {
        push_lines(&mut region, &base_lines[cursor..change.base_start]);
        push_lines(&mut region, &change.replacement);
        cursor = change.base_end;
    }

    push_lines(&mut region, &base_lines[cursor..end]);
    region
}

fn render_conflict_hunk(local_body: &str, remote_body: &str) -> String {
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct LineChange {
    base_start: usize,
    base_end: usize,
    replacement: Vec<String>,
}

fn line_changes(base_body: &str, edited_body: &str) -> Option<Vec<LineChange>> {
    let base_lines = split_lines(base_body);
    let edited_lines = split_lines(edited_body);
    let pairs = lcs_pairs(&base_lines, &edited_lines)?;
    let mut changes = Vec::new();
    let mut base_cursor = 0;
    let mut edited_cursor = 0;

    for (base_index, edited_index) in pairs {
        push_line_change(
            &mut changes,
            &base_lines,
            &edited_lines,
            base_cursor,
            base_index,
            edited_cursor,
            edited_index,
        );
        base_cursor = base_index + 1;
        edited_cursor = edited_index + 1;
    }

    push_line_change(
        &mut changes,
        &base_lines,
        &edited_lines,
        base_cursor,
        base_lines.len(),
        edited_cursor,
        edited_lines.len(),
    );

    Some(changes)
}

fn push_line_change(
    changes: &mut Vec<LineChange>,
    base_lines: &[String],
    edited_lines: &[String],
    base_start: usize,
    base_end: usize,
    edited_start: usize,
    edited_end: usize,
) {
    if base_start == base_end && edited_start == edited_end {
        return;
    }

    if base_lines[base_start..base_end] == edited_lines[edited_start..edited_end] {
        return;
    }

    changes.push(LineChange {
        base_start,
        base_end,
        replacement: edited_lines[edited_start..edited_end].to_vec(),
    });
}

fn lcs_pairs(base_lines: &[String], edited_lines: &[String]) -> Option<Vec<(usize, usize)>> {
    const MAX_DIFF_CELLS: usize = 16_000_000;

    let width = edited_lines.len() + 1;
    let cells = (base_lines.len() + 1).checked_mul(width)?;
    if cells > MAX_DIFF_CELLS {
        return None;
    }

    let mut lengths = vec![0_u32; cells];
    for base_index in (0..base_lines.len()).rev() {
        for edited_index in (0..edited_lines.len()).rev() {
            let index = base_index * width + edited_index;
            lengths[index] = if base_lines[base_index] == edited_lines[edited_index] {
                lengths[(base_index + 1) * width + edited_index + 1] + 1
            } else {
                lengths[(base_index + 1) * width + edited_index]
                    .max(lengths[base_index * width + edited_index + 1])
            };
        }
    }

    let mut pairs = Vec::new();
    let mut base_index = 0;
    let mut edited_index = 0;

    while base_index < base_lines.len() && edited_index < edited_lines.len() {
        if base_lines[base_index] == edited_lines[edited_index] {
            pairs.push((base_index, edited_index));
            base_index += 1;
            edited_index += 1;
        } else if lengths[(base_index + 1) * width + edited_index]
            >= lengths[base_index * width + edited_index + 1]
        {
            base_index += 1;
        } else {
            edited_index += 1;
        }
    }

    Some(pairs)
}

fn split_lines(body: &str) -> Vec<String> {
    body.split_inclusive('\n').map(str::to_string).collect()
}

fn push_lines(output: &mut String, lines: &[String]) {
    for line in lines {
        output.push_str(line);
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

#[cfg(test)]
mod tests {
    use super::{
        CONFLICT_LOCAL_MARKER, CONFLICT_REMOTE_MARKER, CONFLICT_SEPARATOR_MARKER,
        render_conflict_marker_body_with_base,
    };

    #[test]
    fn base_aware_conflict_markers_are_split_by_hunk() {
        let base = "# Roadmap\n\nOld intro.\n\nKeep middle.\n\nOld outro.\n";
        let local = "# Roadmap\n\nLocal intro.\n\nKeep middle.\n\nLocal outro.\n";
        let remote = "# Roadmap\n\nRemote intro.\n\nKeep middle.\n\nRemote outro.\n";

        let merged = render_conflict_marker_body_with_base(base, local, remote);

        assert_eq!(merged.matches(CONFLICT_LOCAL_MARKER).count(), 2);
        assert_eq!(
            merged,
            concat!(
                "# Roadmap\n\n",
                "<<<<<<< LOCAL\n",
                "Local intro.\n",
                "=======\n",
                "Remote intro.\n",
                ">>>>>>> REMOTE\n\n",
                "Keep middle.\n\n",
                "<<<<<<< LOCAL\n",
                "Local outro.\n",
                "=======\n",
                "Remote outro.\n",
                ">>>>>>> REMOTE\n",
            )
        );
    }

    #[test]
    fn base_aware_conflict_markers_merge_non_overlapping_changes() {
        let base = "Intro.\n\nOld middle.\n\nFooter.\n";
        let local = "Intro.\n\nLocal middle.\n\nFooter.\n";
        let remote = "Remote intro.\n\nOld middle.\n\nFooter.\n";

        let merged = render_conflict_marker_body_with_base(base, local, remote);

        assert!(!merged.contains(CONFLICT_LOCAL_MARKER));
        assert_eq!(merged, "Remote intro.\n\nLocal middle.\n\nFooter.\n");
    }

    #[test]
    fn base_aware_conflict_markers_accept_identical_edits() {
        let base = "Intro.\n\nOld body.\n";
        let local = "Intro.\n\nShared body.\n";
        let remote = "Intro.\n\nShared body.\n";

        let merged = render_conflict_marker_body_with_base(base, local, remote);

        assert!(!merged.contains(CONFLICT_LOCAL_MARKER));
        assert!(!merged.contains(CONFLICT_SEPARATOR_MARKER));
        assert!(!merged.contains(CONFLICT_REMOTE_MARKER));
        assert_eq!(merged, local);
    }
}
