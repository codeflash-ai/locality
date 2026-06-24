//! Validation reports and connector-agnostic canonical-document checks.
//!
//! Connectors own schema-specific validation, but the core owns checks that are
//! universal to Locality semantics. The first such check is directive integrity:
//! anchored directive lines may move or be deleted, but edited directive payloads
//! and invented anchors are rejected before push planning.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::canonical::ParsedCanonicalDocument;
use crate::model::{CanonicalBlock, CanonicalDocument, EntityKind, RemoteId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub code: String,
    pub file: PathBuf,
    pub line: Option<usize>,
    pub message: String,
    pub suggested_fix: Option<String>,
}

impl ValidationIssue {
    pub fn new(
        code: impl Into<String>,
        file: impl Into<PathBuf>,
        line: Option<usize>,
        message: impl Into<String>,
        suggested_fix: Option<String>,
    ) -> Self {
        Self {
            code: code.into(),
            file: file.into(),
            line,
            message: message.into(),
            suggested_fix,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    pub fn clean() -> Self {
        Self { issues: Vec::new() }
    }

    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }

    pub fn push(&mut self, issue: ValidationIssue) {
        self.issues.push(issue);
    }

    pub fn extend(&mut self, other: ValidationReport) {
        self.issues.extend(other.issues);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectiveIntegrity {
    Intact,
    Moved,
    Deleted,
    Mangled,
}

pub fn validate_frontmatter_identity(
    parsed: &ParsedCanonicalDocument,
    file: impl Into<PathBuf>,
) -> ValidationReport {
    let file = file.into();
    let mut report = ValidationReport::clean();
    let Some(loc) = parsed.frontmatter.loc.as_ref() else {
        report.push(issue(
            "frontmatter_missing_loc",
            file,
            Some(1),
            "frontmatter is missing the `loc` identity block",
            "restore the generated `loc` frontmatter block before pushing",
        ));
        return report;
    };

    if loc.id.is_none() {
        report.push(issue(
            "frontmatter_missing_loc_id",
            file.clone(),
            Some(1),
            "frontmatter is missing `loc.id`",
            "restore the generated remote id before pushing",
        ));
    }

    match loc.entity_type.as_ref() {
        None => report.push(issue(
            "frontmatter_missing_loc_type",
            file.clone(),
            Some(1),
            "frontmatter is missing `loc.type`",
            "restore the generated entity type before pushing",
        )),
        Some(EntityKind::Unknown(entity_type)) => report.push(issue(
            "frontmatter_unknown_loc_type",
            file.clone(),
            Some(1),
            format!("frontmatter has unknown `loc.type` value `{entity_type}`"),
            "use a supported Locality entity type",
        )),
        Some(_) => {}
    }

    if loc.synced_at.is_none() {
        report.push(issue(
            "frontmatter_missing_synced_at",
            file.clone(),
            Some(1),
            "frontmatter is missing `loc.synced_at`",
            "restore the generated sync timestamp before pushing",
        ));
    }

    if loc.remote_edited_at.is_none() {
        report.push(issue(
            "frontmatter_missing_remote_edited_at",
            file.clone(),
            Some(1),
            "frontmatter is missing `loc.remote_edited_at`",
            "restore the generated remote edit timestamp before pushing",
        ));
    }

    if parsed
        .frontmatter
        .title
        .as_ref()
        .is_none_or(|title| title.trim().is_empty())
    {
        report.push(issue(
            "frontmatter_missing_title",
            file,
            Some(1),
            "frontmatter is missing `title`",
            "restore the page title in frontmatter before pushing",
        ));
    }

    report
}

pub fn validate_directive_syntax(
    parsed: &ParsedCanonicalDocument,
    file: impl Into<PathBuf>,
) -> ValidationReport {
    let file = file.into();
    let mut report = ValidationReport::clean();

    for directive in &parsed.directives {
        if directive.malformed {
            report.push(issue(
                "directive_malformed",
                file.clone(),
                Some(directive.line),
                "Locality directive syntax is malformed",
                "restore the directive line exactly or delete it to delete the block",
            ));
            continue;
        }

        if directive.remote_id.is_none() {
            report.push(issue(
                "directive_missing_id",
                file.clone(),
                Some(directive.line),
                "Locality directive is missing an `id` attribute",
                "restore the directive line exactly or delete it to delete the block",
            ));
        }

        if directive.directive_type.is_none() {
            report.push(issue(
                "directive_missing_type",
                file.clone(),
                Some(directive.line),
                "Locality directive is missing a `type` attribute",
                "restore the directive line exactly or delete it to delete the block",
            ));
        }
    }

    report
}

pub fn validate_directive_integrity(
    shadow: &CanonicalDocument,
    edited: &CanonicalDocument,
    file: impl Into<PathBuf>,
) -> ValidationReport {
    let file = file.into();
    let shadow_directives = directives_by_id(shadow);
    let mut report = ValidationReport::clean();

    for block in edited.blocks.iter().filter(|block| {
        matches!(
            block.kind,
            crate::model::BlockKind::Directive {
                directive_type: _,
                raw: _
            }
        )
    }) {
        let Some((remote_id, directive_type, raw)) = block.directive_parts() else {
            report.push(issue(
                "directive_missing_id",
                file.clone(),
                block.source_span.as_ref().map(|span| span.start_line),
                "directive block is missing an Locality remote id",
                "restore the directive from the shadow copy or remove it as an explicit delete",
            ));
            continue;
        };

        let Some(shadow_block) = shadow_directives.get(remote_id) else {
            report.push(issue(
                "directive_unknown",
                file.clone(),
                block.source_span.as_ref().map(|span| span.start_line),
                format!(
                    "directive anchor `{}` was not present in the synced shadow",
                    remote_id.0
                ),
                "remove invented directive anchors; create normal Markdown instead",
            ));
            continue;
        };

        let Some((_, shadow_type, shadow_raw)) = shadow_block.directive_parts() else {
            continue;
        };

        if directive_type != shadow_type || raw != shadow_raw {
            report.push(issue(
                "directive_mangled",
                file.clone(),
                block.source_span.as_ref().map(|span| span.start_line),
                format!("directive anchor `{}` was edited", remote_id.0),
                "restore the directive line exactly, move it as a whole line, or delete it to delete the block",
            ));
        }
    }

    report
}

pub fn classify_directive_change(
    shadow: &CanonicalBlock,
    edited: Option<&CanonicalBlock>,
) -> DirectiveIntegrity {
    match (
        shadow.directive_parts(),
        edited.and_then(CanonicalBlock::directive_parts),
    ) {
        (Some(_), None) => DirectiveIntegrity::Deleted,
        (
            Some((shadow_id, shadow_type, shadow_raw)),
            Some((edited_id, edited_type, edited_raw)),
        ) if shadow_id == edited_id && shadow_type == edited_type && shadow_raw == edited_raw => {
            if shadow.source_span != edited.and_then(|block| block.source_span.clone()) {
                DirectiveIntegrity::Moved
            } else {
                DirectiveIntegrity::Intact
            }
        }
        (Some(_), Some(_)) => DirectiveIntegrity::Mangled,
        _ => DirectiveIntegrity::Intact,
    }
}

fn directives_by_id(document: &CanonicalDocument) -> BTreeMap<&RemoteId, &CanonicalBlock> {
    document
        .blocks
        .iter()
        .filter_map(|block| {
            block
                .directive_parts()
                .map(|(remote_id, _, _)| (remote_id, block))
        })
        .collect()
}

fn issue(
    code: impl Into<String>,
    file: PathBuf,
    line: Option<usize>,
    message: impl Into<String>,
    suggested_fix: impl Into<String>,
) -> ValidationIssue {
    ValidationIssue {
        code: code.into(),
        file,
        line,
        message: message.into(),
        suggested_fix: Some(suggested_fix.into()),
    }
}
