use locality_core::model::{CanonicalDocument, RemoteId};
use locality_core::shadow::ShadowDocument;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::docs_dto::{
    GoogleDocument, Paragraph, ParagraphElement, StructuralElement, Table, TextStyle,
};
use crate::drive_dto::DriveFile;
use crate::oauth::GOOGLE_DOCS_CONNECTOR_ID;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleDocsNativeBundle {
    pub drive_file: DriveFile,
    pub document: GoogleDocument,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleDocsRenderedEntity {
    pub document: CanonicalDocument,
    pub shadow: ShadowDocument,
    pub push_blocking_directives: bool,
}

pub fn render_google_document(
    bundle: &GoogleDocsNativeBundle,
) -> LocalityResult<GoogleDocsRenderedEntity> {
    let mut rendered_blocks = Vec::new();
    let mut native_block_ids = Vec::new();
    let mut native_block_kinds = Vec::new();
    let mut push_blocking_directives = false;

    for element in &bundle.document.body.content {
        let block_id = element_block_id(&bundle.document.document_id, element);
        if let Some(paragraph) = &element.paragraph {
            let has_unsupported_inline = paragraph_has_unsupported_inline(paragraph);
            let paragraph = render_paragraph(&bundle.document, paragraph);
            if !paragraph.trim().is_empty() {
                rendered_blocks.push(paragraph);
                native_block_ids.push(RemoteId::new(block_id));
                native_block_kinds.push(None);
            }
            if has_unsupported_inline {
                push_blocking_directives = true;
                rendered_blocks.push(format!(
                    "::loc{{id={}:unsupported type=google_docs_unsupported kind=\"inline_element\"}}",
                    element_block_id(&bundle.document.document_id, element)
                ));
            }
        } else if let Some(table) = &element.table {
            let table = render_table(table);
            if !table.trim().is_empty() {
                rendered_blocks.push(table);
                native_block_ids.push(RemoteId::new(block_id));
                native_block_kinds.push(Some("google_docs_table".to_string()));
            }
        } else if unsupported_structural_element(element) {
            push_blocking_directives = true;
            rendered_blocks.push(format!(
                "::loc{{id={} type=google_docs_unsupported kind=\"{}\"}}",
                block_id,
                unsupported_kind(element)
            ));
        }
    }

    let body = if rendered_blocks.is_empty() {
        String::new()
    } else {
        format!("{}\n", rendered_blocks.join("\n\n"))
    };
    let frontmatter = document_frontmatter(
        &bundle.drive_file,
        bundle.document.revision_id.as_deref().unwrap_or(""),
    );
    let document = CanonicalDocument::new(frontmatter.clone(), body.clone());
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new(bundle.document.document_id.clone()),
        body,
        1,
        native_block_ids,
    )
    .map_err(|error| LocalityError::InvalidState(error.to_string()))?
    .with_frontmatter(frontmatter);
    let mut native_block_kinds = native_block_kinds.into_iter();
    for block in &mut shadow.blocks {
        if !block.kind.is_directive() {
            block.native_kind = native_block_kinds.next().flatten();
        }
    }

    Ok(GoogleDocsRenderedEntity {
        document,
        shadow,
        push_blocking_directives,
    })
}

pub fn document_frontmatter(file: &DriveFile, docs_revision_id: &str) -> String {
    let version = combined_remote_version(file, Some(docs_revision_id));
    format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\n",
        yaml_scalar(&file.id),
        GOOGLE_DOCS_CONNECTOR_ID,
        yaml_scalar(&version),
        yaml_scalar(&version),
        yaml_scalar(&file.name)
    )
}

pub fn combined_remote_version(file: &DriveFile, docs_revision_id: Option<&str>) -> String {
    match (
        file.remote_version(),
        docs_revision_id.filter(|revision| !revision.is_empty()),
    ) {
        (Some(drive), Some(revision)) => format!("{drive}|docs:{revision}"),
        (Some(drive), None) => drive,
        (None, Some(revision)) => format!("docs:{revision}"),
        (None, None) => "unknown".to_string(),
    }
}

fn render_paragraph(document: &GoogleDocument, paragraph: &Paragraph) -> String {
    let text = paragraph_text(&paragraph.elements);
    let text = trim_docs_newline(&text);
    if text.trim().is_empty() {
        return String::new();
    }

    if let Some(bullet) = &paragraph.bullet {
        let nesting = bullet.nesting_level.unwrap_or_default();
        let indent = "  ".repeat(nesting);
        let marker = bullet
            .list_id
            .as_ref()
            .and_then(|list_id| document.lists.get(list_id))
            .and_then(|list| list.list_properties.nesting_levels.get(nesting))
            .and_then(|level| level.glyph_type.as_deref())
            .map(list_marker)
            .unwrap_or("-");
        return format!("{indent}{marker} {text}");
    }

    match paragraph
        .paragraph_style
        .as_ref()
        .and_then(|style| style.named_style_type.as_deref())
        .and_then(heading_level)
    {
        Some(level) => format!("{} {}", "#".repeat(level), text),
        None => text.to_string(),
    }
}

fn paragraph_text(elements: &[ParagraphElement]) -> String {
    elements
        .iter()
        .filter_map(|element| element.text_run.as_ref())
        .map(|text_run| render_text_run(&text_run.content, &text_run.text_style))
        .collect::<String>()
}

fn render_text_run(content: &str, style: &TextStyle) -> String {
    let mut rendered = trim_docs_newline(content).to_string();
    if rendered.is_empty() {
        return rendered;
    }
    if style.bold {
        rendered = format!("**{rendered}**");
    }
    if style.italic {
        rendered = format!("*{rendered}*");
    }
    if style.underline {
        rendered = format!("<u>{rendered}</u>");
    }
    if style.strikethrough {
        rendered = format!("~~{rendered}~~");
    }
    if let Some(url) = style.link.as_ref().and_then(|link| link.url.as_deref()) {
        rendered = format!("[{rendered}]({url})");
    }
    rendered
}

fn render_table(table: &Table) -> String {
    let rows = table
        .table_rows
        .iter()
        .map(|row| {
            row.table_cells
                .iter()
                .map(|cell| {
                    cell.content
                        .iter()
                        .filter_map(|element| element.paragraph.as_ref())
                        .map(|paragraph| {
                            trim_docs_newline(&paragraph_text(&paragraph.elements)).to_string()
                        })
                        .collect::<Vec<_>>()
                        .join("<br>")
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return String::new();
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return String::new();
    }
    let mut normalized = rows
        .into_iter()
        .map(|mut row| {
            row.resize(width, String::new());
            row
        })
        .collect::<Vec<_>>();
    if normalized.len() == 1 {
        normalized.push(vec![String::new(); width]);
    }

    let header = markdown_table_row(&normalized[0]);
    let separator = markdown_table_row(&vec!["---".to_string(); width]);
    let body = normalized[1..]
        .iter()
        .map(|row| markdown_table_row(row))
        .collect::<Vec<_>>();

    std::iter::once(header)
        .chain(std::iter::once(separator))
        .chain(body)
        .collect::<Vec<_>>()
        .join("\n")
}

fn markdown_table_row(cells: &[String]) -> String {
    format!("| {} |", cells.join(" | "))
}

fn heading_level(style: &str) -> Option<usize> {
    match style {
        "HEADING_1" => Some(1),
        "HEADING_2" => Some(2),
        "HEADING_3" => Some(3),
        "HEADING_4" => Some(4),
        "HEADING_5" => Some(5),
        "HEADING_6" => Some(6),
        _ => None,
    }
}

fn list_marker(glyph_type: &str) -> &'static str {
    if glyph_type.contains("DECIMAL") || glyph_type.contains("NUMBER") {
        "1."
    } else {
        "-"
    }
}

fn trim_docs_newline(value: &str) -> &str {
    value.trim_end_matches(['\r', '\n'])
}

fn unsupported_structural_element(element: &StructuralElement) -> bool {
    element.section_break.is_some() || element.table_of_contents.is_some()
}

fn paragraph_has_unsupported_inline(paragraph: &Paragraph) -> bool {
    paragraph.elements.iter().any(|element| {
        element.inline_object_element.is_some()
            || element.page_break.is_some()
            || element.footnote_reference.is_some()
            || element.equation.is_some()
    })
}

fn unsupported_kind(element: &StructuralElement) -> &'static str {
    if element.section_break.is_some() {
        "section_break"
    } else if element.table_of_contents.is_some() {
        "table_of_contents"
    } else {
        "unknown"
    }
}

fn element_block_id(document_id: &str, element: &StructuralElement) -> String {
    format!(
        "{}:{}:{}",
        document_id,
        element.start_index.unwrap_or_default(),
        element.end_index.unwrap_or_default()
    )
}

fn yaml_scalar(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.' | '/' | ' '))
        && !value.is_empty()
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use locality_core::model::CanonicalDocument;

    use super::{GoogleDocsNativeBundle, render_google_document};
    use crate::drive_dto::DriveFile;

    #[test]
    fn renders_common_google_docs_structures_to_markdown() {
        let bundle = GoogleDocsNativeBundle {
            drive_file: drive_file("doc-1", "Launch Brief"),
            document: serde_json::from_value(serde_json::json!({
                "documentId": "doc-1",
                "title": "Launch Brief",
                "revisionId": "rev-1",
                "body": {
                    "content": [
                        { "startIndex": 1, "endIndex": 14, "paragraph": {
                            "paragraphStyle": { "namedStyleType": "HEADING_1" },
                            "elements": [{ "textRun": { "content": "Launch Brief\n" } }]
                        }},
                        { "startIndex": 14, "endIndex": 33, "paragraph": {
                            "elements": [
                                { "textRun": { "content": "Hello " } },
                                { "textRun": { "content": "world", "textStyle": { "bold": true, "link": { "url": "https://example.test" } } } },
                                { "textRun": { "content": "\n" } }
                            ]
                        }},
                        { "startIndex": 33, "endIndex": 39, "paragraph": {
                            "bullet": { "listId": "list-1", "nestingLevel": 0 },
                            "elements": [{ "textRun": { "content": "Item\n" } }]
                        }},
                        { "startIndex": 39, "endIndex": 59, "table": {
                            "tableRows": [
                                { "tableCells": [
                                    { "content": [{ "paragraph": { "elements": [{ "textRun": { "content": "Key\n" } }] } }] },
                                    { "content": [{ "paragraph": { "elements": [{ "textRun": { "content": "Value\n" } }] } }] }
                                ]},
                                { "tableCells": [
                                    { "content": [{ "paragraph": { "elements": [{ "textRun": { "content": "Owner\n" } }] } }] },
                                    { "content": [{ "paragraph": { "elements": [{ "textRun": { "content": "Locality\n" } }] } }] }
                                ]}
                            ]
                        }}
                    ]
                },
                "lists": {
                    "list-1": {
                        "listProperties": {
                            "nestingLevels": [{ "glyphType": "BULLET" }]
                        }
                    }
                }
            }))
            .expect("document"),
        };

        let rendered = render_google_document(&bundle).expect("render");

        assert!(
            rendered
                .document
                .frontmatter
                .contains("connector: google-docs")
        );
        assert!(rendered.document.frontmatter.contains("id: doc-1"));
        assert!(rendered.document.body.contains("# Launch Brief"));
        assert!(
            rendered
                .document
                .body
                .contains("[**world**](https://example.test)")
        );
        assert!(rendered.document.body.contains("- Item"));
        assert!(rendered.document.body.contains("| Key | Value |"));
        let table_block = rendered
            .shadow
            .blocks
            .iter()
            .find(|block| block.text.contains("| Key | Value |"))
            .expect("table shadow block");
        assert_eq!(
            table_block.native_kind.as_deref(),
            Some("google_docs_table")
        );
        assert_eq!(rendered.shadow.entity_id.as_str(), "doc-1");
    }

    #[test]
    fn unsupported_structures_render_as_push_blocking_directives() {
        let bundle = GoogleDocsNativeBundle {
            drive_file: drive_file("doc-1", "Drawing Doc"),
            document: serde_json::from_value(serde_json::json!({
                "documentId": "doc-1",
                "title": "Drawing Doc",
                "revisionId": "rev-1",
                "body": {
                    "content": [
                        { "startIndex": 1, "endIndex": 2, "sectionBreak": {} }
                    ]
                }
            }))
            .expect("document"),
        };

        let rendered = render_google_document(&bundle).expect("render");

        assert!(rendered.document.body.contains("::loc{"));
        assert!(
            rendered
                .document
                .body
                .contains("type=google_docs_unsupported")
        );
        assert!(rendered.push_blocking_directives);
    }

    #[test]
    fn unsupported_inline_elements_render_as_push_blocking_directives() {
        let bundle = GoogleDocsNativeBundle {
            drive_file: drive_file("doc-1", "Inline Object Doc"),
            document: serde_json::from_value(serde_json::json!({
                "documentId": "doc-1",
                "title": "Inline Object Doc",
                "revisionId": "rev-1",
                "body": {
                    "content": [
                        { "startIndex": 1, "endIndex": 20, "paragraph": {
                            "elements": [
                                { "textRun": { "content": "Before " } },
                                { "inlineObjectElement": { "inlineObjectId": "obj-1" } },
                                { "textRun": { "content": "after\n" } }
                            ]
                        }}
                    ]
                }
            }))
            .expect("document"),
        };

        let rendered = render_google_document(&bundle).expect("render");

        assert!(rendered.document.body.contains("Before after"));
        assert!(
            rendered
                .document
                .body
                .contains("type=google_docs_unsupported")
        );
        assert!(rendered.push_blocking_directives);
    }

    fn drive_file(id: &str, name: &str) -> DriveFile {
        DriveFile {
            id: id.to_string(),
            name: name.to_string(),
            mime_type: crate::drive_dto::DRIVE_GOOGLE_DOC_MIME_TYPE.to_string(),
            parents: vec!["folder-1".to_string()],
            modified_time: Some("2026-06-25T10:00:00.000Z".to_string()),
            version: Some("7".to_string()),
            trashed: false,
        }
    }

    #[test]
    fn stub_frontmatter_uses_connector_neutral_identity() {
        let file = drive_file("doc-1", "Launch Brief");

        let frontmatter = super::document_frontmatter(&file, "rev-1");

        assert!(frontmatter.contains("loc:"));
        assert!(frontmatter.contains("connector: google-docs"));
        assert!(frontmatter.contains("title: Launch Brief"));
    }

    #[test]
    fn document_body_still_uses_standard_stub_marker_when_unhydrated() {
        assert_eq!(
            CanonicalDocument::empty_stub().body.trim(),
            CanonicalDocument::STUB_MARKER
        );
    }
}
