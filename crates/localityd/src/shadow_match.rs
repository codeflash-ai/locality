use locality_core::canonical::{
    Frontmatter, LocalityMetadata, ParsedCanonicalDocument, parse_canonical_markdown,
    render_canonical_markdown,
};
use locality_core::diff::{BlockDiffEngine, DiffEngine};
use locality_core::model::CanonicalDocument;
use locality_core::shadow::{ShadowDocument, rendered_bodies_equivalent};

pub(crate) fn parsed_matches_shadow(
    parsed: &ParsedCanonicalDocument,
    shadow: &ShadowDocument,
) -> bool {
    if !parsed_body_matches_shadow(parsed, shadow) {
        return false;
    }

    let Some(shadow_parsed) = parse_shadow(shadow) else {
        return false;
    };

    frontmatter_matches_ignoring_sync_metadata(&parsed.frontmatter, &shadow_parsed.frontmatter)
}

fn parsed_body_matches_shadow(parsed: &ParsedCanonicalDocument, shadow: &ShadowDocument) -> bool {
    rendered_bodies_equivalent(&parsed.document.body, &shadow.rendered_body)
        || BlockDiffEngine::new()
            .with_edited_body_start_line(parsed.body_start_line)
            .plan_push(shadow, &parsed.document)
            .map(|plan| plan.operations.is_empty() && plan.degradations.is_empty())
            .unwrap_or(false)
}

pub(crate) fn shadows_match(left: &ShadowDocument, right: &ShadowDocument) -> bool {
    if !rendered_bodies_equivalent(&left.rendered_body, &right.rendered_body) {
        return false;
    }

    let (Some(left), Some(right)) = (parse_shadow(left), parse_shadow(right)) else {
        return false;
    };

    frontmatter_matches_ignoring_sync_metadata(&left.frontmatter, &right.frontmatter)
}

fn parse_shadow(shadow: &ShadowDocument) -> Option<ParsedCanonicalDocument> {
    parse_canonical_markdown(&render_canonical_markdown(&CanonicalDocument::new(
        shadow.frontmatter.clone(),
        shadow.rendered_body.clone(),
    )))
    .ok()
}

fn frontmatter_matches_ignoring_sync_metadata(left: &Frontmatter, right: &Frontmatter) -> bool {
    loc_metadata_matches_ignoring_sync_metadata(&left.loc, &right.loc)
        && left.title == right.title
        && left.properties == right.properties
}

fn loc_metadata_matches_ignoring_sync_metadata(
    left: &Option<LocalityMetadata>,
    right: &Option<LocalityMetadata>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.id == right.id
                && left.entity_type == right.entity_type
                && left.raw_entity_type == right.raw_entity_type
                && left.parent == right.parent
        }
        _ => false,
    }
}
