use locality_core::model::{CanonicalDocument, RemoteId};
use locality_core::shadow::ShadowDocument;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::docs_dto::{
    GoogleDocument, InlineObjectElement, Paragraph, ParagraphElement, StructuralElement, Table,
    TextStyle,
};
use crate::drive_dto::DriveFile;
use crate::oauth::GOOGLE_DOCS_CONNECTOR_ID;

pub const GOOGLE_DOCS_INLINE_OBJECT_NATIVE_KIND: &str = "google_docs_inline_object";

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
            let paragraph = render_paragraph(&bundle.document, paragraph);
            if !paragraph.text.trim().is_empty() {
                rendered_blocks.push(paragraph.text);
                native_block_ids.push(RemoteId::new(block_id));
                native_block_kinds.push(
                    paragraph
                        .has_rendered_inline_object
                        .then(|| GOOGLE_DOCS_INLINE_OBJECT_NATIVE_KIND.to_string()),
                );
            }
            if paragraph.has_unsupported_inline {
                push_blocking_directives = true;
                rendered_blocks.push(format!(
                    "::loc{{id={}:unsupported type=google_docs_unsupported kind=\"inline_element\"}}",
                    element_block_id(&bundle.document.document_id, element)
                ));
            }
        } else if let Some(table) = &element.table {
            let table = render_table(&bundle.document, table);
            if !table.trim().is_empty() {
                rendered_blocks.push(table);
                native_block_ids.push(RemoteId::new(block_id));
                native_block_kinds.push(Some("google_docs_table".to_string()));
            }
        } else if unsupported_structural_element(element) {
            if implicit_document_boundary_section_break(element) {
                continue;
            }
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RenderedParagraph {
    text: String,
    has_unsupported_inline: bool,
    has_rendered_inline_object: bool,
}

fn render_paragraph(document: &GoogleDocument, paragraph: &Paragraph) -> RenderedParagraph {
    let inline = paragraph_text(document, &paragraph.elements);
    let text = trim_docs_newline(&inline.text);
    if text.trim().is_empty() {
        return RenderedParagraph {
            text: String::new(),
            has_unsupported_inline: inline.has_unsupported_inline,
            has_rendered_inline_object: inline.has_rendered_inline_object,
        };
    }

    let text = if let Some(bullet) = &paragraph.bullet {
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
        format!("{indent}{marker} {text}")
    } else {
        match paragraph
            .paragraph_style
            .as_ref()
            .and_then(|style| style.named_style_type.as_deref())
            .and_then(heading_level)
        {
            Some(level) => format!("{} {}", "#".repeat(level), text),
            None => text.to_string(),
        }
    };

    RenderedParagraph {
        text,
        has_unsupported_inline: inline.has_unsupported_inline,
        has_rendered_inline_object: inline.has_rendered_inline_object,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RenderedInlineContent {
    text: String,
    has_unsupported_inline: bool,
    has_rendered_inline_object: bool,
}

fn paragraph_text(
    document: &GoogleDocument,
    elements: &[ParagraphElement],
) -> RenderedInlineContent {
    let mut rendered = RenderedInlineContent::default();
    for element in elements {
        if let Some(text_run) = element.text_run.as_ref() {
            rendered
                .text
                .push_str(&render_text_run(&text_run.content, &text_run.text_style));
        }
        if let Some(inline_object) = element.inline_object_element.as_ref() {
            if let Some(image) = render_inline_image(document, inline_object) {
                rendered.text.push_str(&image);
                rendered.has_rendered_inline_object = true;
            } else {
                rendered.has_unsupported_inline = true;
            }
        }
        if element.page_break.is_some()
            || element.footnote_reference.is_some()
            || element.equation.is_some()
        {
            rendered.has_unsupported_inline = true;
        }
    }
    rendered
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

fn render_table(document: &GoogleDocument, table: &Table) -> String {
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
                            trim_docs_newline(&paragraph_text(document, &paragraph.elements).text)
                                .to_string()
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

fn implicit_document_boundary_section_break(element: &StructuralElement) -> bool {
    element.section_break.is_some()
        && element.start_index.unwrap_or_default() == 0
        && element.end_index == Some(1)
}

fn render_inline_image(
    document: &GoogleDocument,
    inline_object: &InlineObjectElement,
) -> Option<String> {
    let object_id = inline_object.inline_object_id.as_deref()?;
    let embedded_object = document
        .inline_objects
        .get(object_id)?
        .inline_object_properties
        .embedded_object
        .as_ref()?;
    let content_uri = embedded_object
        .image_properties
        .as_ref()?
        .content_uri
        .as_deref()?;
    let alt = embedded_object
        .description
        .as_deref()
        .or(embedded_object.title.as_deref())
        .unwrap_or("Google Docs image");
    Some(format!(
        "![{}]({})",
        markdown_image_alt(alt),
        markdown_image_target(content_uri)
    ))
}

fn markdown_image_alt(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

fn markdown_image_target(value: &str) -> String {
    value.replace(')', "%29")
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
    fn initial_document_section_break_is_not_rendered_as_unsupported_content() {
        let bundle = GoogleDocsNativeBundle {
            drive_file: drive_file("doc-1", "Plain Doc"),
            document: serde_json::from_value(serde_json::json!({
                "documentId": "doc-1",
                "title": "Plain Doc",
                "revisionId": "rev-1",
                "body": {
                    "content": [
                        { "endIndex": 1, "sectionBreak": {} },
                        { "startIndex": 1, "endIndex": 12, "paragraph": {
                            "elements": [{ "textRun": { "content": "Hello doc\n" } }]
                        }}
                    ]
                }
            }))
            .expect("document"),
        };

        let rendered = render_google_document(&bundle).expect("render");

        assert_eq!(rendered.document.body, "Hello doc\n");
        assert!(!rendered.document.body.contains("google_docs_unsupported"));
        assert!(!rendered.push_blocking_directives);
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

    #[test]
    fn inline_image_objects_render_as_markdown_images() {
        let bundle = GoogleDocsNativeBundle {
            drive_file: drive_file("doc-1", "Logo Doc"),
            document: serde_json::from_value(serde_json::json!({
                "documentId": "doc-1",
                "title": "Logo Doc",
                "revisionId": "rev-1",
                "body": {
                    "content": [
                        { "startIndex": 2, "endIndex": 4, "paragraph": {
                            "elements": [
                                { "startIndex": 2, "endIndex": 3, "inlineObjectElement": { "inlineObjectId": "obj-1" } },
                                { "startIndex": 3, "endIndex": 4, "textRun": { "content": "\n" } }
                            ]
                        }}
                    ]
                },
                "inlineObjects": {
                    "obj-1": {
                        "objectId": "obj-1",
                        "inlineObjectProperties": {
                            "embeddedObject": {
                                "description": "A circle with logo written in the center",
                                "imageProperties": {
                                    "contentUri": "https://example.test/circle.png"
                                }
                            }
                        }
                    }
                }
            }))
            .expect("document"),
        };

        let rendered = render_google_document(&bundle).expect("render");

        assert_eq!(
            rendered.document.body,
            "![A circle with logo written in the center](https://example.test/circle.png)\n"
        );
        assert!(!rendered.document.body.contains("google_docs_unsupported"));
        assert!(!rendered.push_blocking_directives);
        assert_eq!(
            rendered.shadow.blocks[0].native_kind.as_deref(),
            Some("google_docs_inline_object")
        );
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
