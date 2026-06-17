use afs_core::canonical::{
    CanonicalParseErrorKind, parse_canonical_markdown, parse_directive_line,
    render_canonical_markdown,
};
use afs_core::model::{CanonicalDocument, EntityKind, RemoteId};
use afs_core::validation::{validate_directive_syntax, validate_frontmatter_identity};

const VALID_DOC: &str = r#"---
afs:
  id: page-1
  type: page
  parent: parent-1
  synced_at: "2026-06-09T14:02:11Z"
  remote_edited_at: "2026-06-09T13:58:40Z"
title: Roadmap 2026
status: In progress
---
# Roadmap 2026

::afs{id=block-1 type=synced_block title="Shared header"}

Q2 priorities are...
"#;

#[test]
fn parses_frontmatter_identity_directives_and_renders_stably() {
    let parsed = parse_canonical_markdown(VALID_DOC).expect("valid canonical document");

    let afs = parsed.frontmatter.afs.as_ref().expect("afs metadata");
    assert_eq!(afs.id, Some(RemoteId::new("page-1")));
    assert_eq!(afs.entity_type, Some(EntityKind::Page));
    assert_eq!(afs.parent, Some(RemoteId::new("parent-1")));
    assert_eq!(parsed.frontmatter.title.as_deref(), Some("Roadmap 2026"));
    assert!(parsed.frontmatter.properties.contains_key("status"));
    assert!(!parsed.is_stub());
    assert_eq!(parsed.body_start_line, 11);

    assert_eq!(parsed.directives.len(), 1);
    let directive = &parsed.directives[0];
    assert_eq!(directive.remote_id, Some(RemoteId::new("block-1")));
    assert_eq!(directive.directive_type.as_deref(), Some("synced_block"));
    assert_eq!(directive.title.as_deref(), Some("Shared header"));
    assert_eq!(directive.line, 13);
    assert!(!directive.malformed);
    assert_eq!(
        parsed.document.blocks[0]
            .source_span
            .as_ref()
            .unwrap()
            .start_line,
        13
    );

    assert_eq!(render_canonical_markdown(&parsed.document), VALID_DOC);
    assert!(validate_frontmatter_identity(&parsed, "Roadmap.md").is_clean());
    assert!(validate_directive_syntax(&parsed, "Roadmap.md").is_clean());
}

#[test]
fn detects_stub_marker_body() {
    let input = format!(
        "---\nafs:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Stub\n---\n{}\n",
        CanonicalDocument::STUB_MARKER
    );
    let parsed = parse_canonical_markdown(&input).expect("stub document");

    assert!(parsed.is_stub());
}

#[test]
fn rejects_missing_and_unterminated_frontmatter() {
    let missing = parse_canonical_markdown("# No frontmatter").unwrap_err();
    assert_eq!(missing.kind, CanonicalParseErrorKind::MissingFrontmatter);
    assert_eq!(missing.line, Some(1));

    let unterminated = parse_canonical_markdown("---\ntitle: nope\n").unwrap_err();
    assert_eq!(
        unterminated.kind,
        CanonicalParseErrorKind::UnterminatedFrontmatter
    );
}

#[test]
fn rejects_invalid_yaml_frontmatter() {
    let error = parse_canonical_markdown("---\nafs: [\n---\nbody").unwrap_err();

    assert_eq!(error.kind, CanonicalParseErrorKind::InvalidFrontmatterYaml);
    assert!(error.line.is_some());
}

#[test]
fn validates_required_frontmatter_identity_fields() {
    let input = "---\nafs:\n  id: ''\n  type: unknown\n---\nbody";
    let parsed = parse_canonical_markdown(input).expect("parseable document");
    let report = validate_frontmatter_identity(&parsed, "bad.md");
    let codes: Vec<_> = report
        .issues
        .iter()
        .map(|issue| issue.code.as_str())
        .collect();

    assert_eq!(
        codes,
        vec![
            "frontmatter_missing_afs_id",
            "frontmatter_unknown_afs_type",
            "frontmatter_missing_synced_at",
            "frontmatter_missing_remote_edited_at",
            "frontmatter_missing_title",
        ]
    );
}

#[test]
fn validates_missing_afs_block() {
    let parsed =
        parse_canonical_markdown("---\ntitle: Missing AFS\n---\nbody").expect("parseable document");
    let report = validate_frontmatter_identity(&parsed, "missing.md");

    assert_eq!(report.issues.len(), 1);
    assert_eq!(report.issues[0].code, "frontmatter_missing_afs");
}

#[test]
fn parses_quoted_and_unquoted_directive_attributes() {
    let directive = parse_directive_line(
        r#"::afs{id=b771 type=synced_block title="Shared header"}"#,
        42,
    )
    .expect("directive");

    assert_eq!(directive.remote_id, Some(RemoteId::new("b771")));
    assert_eq!(directive.directive_type.as_deref(), Some("synced_block"));
    assert_eq!(directive.title.as_deref(), Some("Shared header"));
    assert_eq!(directive.line, 42);
}

#[test]
fn parses_escaped_directive_attribute_values() {
    let directive = parse_directive_line(
        r#"::afs{id=media-1 type=image title="Quote: \"hello\" and slash \\"}"#,
        7,
    )
    .expect("directive");

    assert_eq!(directive.remote_id, Some(RemoteId::new("media-1")));
    assert_eq!(directive.directive_type.as_deref(), Some("image"));
    assert_eq!(
        directive.title.as_deref(),
        Some(r#"Quote: "hello" and slash \"#)
    );
    assert_eq!(
        directive.attributes.get("title").map(String::as_str),
        Some(r#"Quote: "hello" and slash \"#)
    );
}

#[test]
fn directive_syntax_validation_reports_malformed_and_missing_fields() {
    let input = "---\nafs:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Bad directives\n---\n::afs{id=block-1 type=synced_block\n::afs{type=synced_block}\n::afs{id=block-2}\n";
    let parsed = parse_canonical_markdown(input).expect("parseable document");
    let report = validate_directive_syntax(&parsed, "bad-directives.md");
    let codes: Vec<_> = report
        .issues
        .iter()
        .map(|issue| (issue.code.as_str(), issue.line))
        .collect();

    assert_eq!(
        codes,
        vec![
            ("directive_malformed", Some(9)),
            ("directive_missing_id", Some(10)),
            ("directive_missing_type", Some(11)),
        ]
    );
}
