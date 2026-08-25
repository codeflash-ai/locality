use locality_core::model::{CanonicalDocument, RemoteId};
use locality_core::shadow::ShadowDocument;
use locality_core::{LocalityError, LocalityResult};

use crate::docs_dto::{
    GoogleDocument, InlineObjectElement, Paragraph, ParagraphElement, StructuralElement, Table,
    TextStyle,
};
use crate::oauth::GOOGLE_DOCS_CONNECTOR_ID;

pub const GOOGLE_DOCS_INLINE_OBJECT_NATIVE_KIND: &str = "google_docs_inline_object";
pub const GOOGLE_DOCS_TABLE_NATIVE_KIND: &str = "google_docs_table";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleDocsRenderedEntity {
    pub document: CanonicalDocument,
    pub shadow: ShadowDocument,
    pub push_blocking_directives: bool,
}

pub fn render_google_document(
    document: &GoogleDocument,
) -> LocalityResult<GoogleDocsRenderedEntity> {
    let mut rendered_blocks = Vec::new();
    let mut native_block_ids = Vec::new();
    let mut native_block_kinds = Vec::new();
    let mut push_blocking_directives = false;

    for element in &document.body.content {
        let block_id = element_block_id(&document.document_id, element);
        if let Some(paragraph) = &element.paragraph {
            let paragraph = render_paragraph(document, paragraph);
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
                    element_block_id(&document.document_id, element)
                ));
            }
        } else if let Some(table) = &element.table {
            let table = render_table(document, table);
            if !table.trim().is_empty() {
                rendered_blocks.push(table);
                native_block_ids.push(RemoteId::new(block_id));
                native_block_kinds.push(Some(GOOGLE_DOCS_TABLE_NATIVE_KIND.to_string()));
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
    let frontmatter = document_frontmatter(document);
    let canonical_document = CanonicalDocument::new(frontmatter.clone(), body.clone());
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new(document.document_id.clone()),
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
        document: canonical_document,
        shadow,
        push_blocking_directives,
    })
}

pub fn document_frontmatter(document: &GoogleDocument) -> String {
    let version = document_remote_version(document);
    format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\n",
        yaml_scalar(&document.document_id),
        GOOGLE_DOCS_CONNECTOR_ID,
        yaml_scalar(&version),
        yaml_scalar(&version),
        yaml_scalar(&document.title)
    )
}

pub fn document_remote_version(document: &GoogleDocument) -> String {
    document
        .revision_id
        .as_deref()
        .filter(|revision| !revision.is_empty())
        .map(|revision| format!("docs:{revision}"))
        .unwrap_or_else(|| "unknown".to_string())
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
            None => escape_paragraph_block_start_marker(text.to_string()),
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
    let mut rendered = escape_markdown_text(&normalize_docs_text(trim_docs_newline(content)));
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
        rendered = format!(
            "[{}]({})",
            escape_markdown_link_label(&rendered),
            escape_markdown_link_href(url)
        );
    }
    rendered
}

fn normalize_docs_text(value: &str) -> String {
    value.replace('\u{000b}', "\n")
}

fn escape_markdown_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    let mut rest = text;

    while !rest.is_empty() {
        if let Some(marker) = literal_inline_marker_prefix(rest) {
            escaped.push('\\');
            escaped.push_str(marker);
            rest = &rest[marker.len()..];
            continue;
        }

        let ch = rest.chars().next().expect("non-empty rest");
        match ch {
            '\\' => escaped.push_str("\\\\"),
            _ => escaped.push(ch),
        }
        rest = &rest[ch.len_utf8()..];
    }

    escaped
}

fn escape_markdown_link_label(text: &str) -> String {
    text.replace(']', "\\]")
}

fn escape_markdown_link_href(href: &str) -> String {
    href.replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

fn escape_paragraph_block_start_marker(text: String) -> String {
    let Some((index, _)) = text
        .char_indices()
        .find(|(_, ch)| !matches!(ch, ' ' | '\t'))
    else {
        return text;
    };

    if paragraph_block_start_marker_needs_escape(&text[index..]) {
        let mut escaped = String::with_capacity(text.len() + 1);
        escaped.push_str(&text[..index]);
        escaped.push('\\');
        escaped.push_str(&text[index..]);
        escaped
    } else {
        text
    }
}

fn paragraph_block_start_marker_needs_escape(value: &str) -> bool {
    value.starts_with("::loc")
        || heading_marker(value)
        || block_list_marker(value)
        || quote_marker(value)
        || divider_marker(value)
}

fn heading_marker(value: &str) -> bool {
    let level = value.chars().take_while(|ch| *ch == '#').count();
    (1..=6).contains(&level) && value[level..].starts_with(char::is_whitespace)
}

fn block_list_marker(value: &str) -> bool {
    value.starts_with("- ")
        || value.starts_with("* ")
        || value.starts_with("+ ")
        || ordered_list_marker(value)
}

fn ordered_list_marker(value: &str) -> bool {
    let digit_count = value.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digit_count > 0 && value[digit_count..].starts_with(". ")
}

fn quote_marker(value: &str) -> bool {
    value.starts_with("> ")
}

fn divider_marker(value: &str) -> bool {
    value.trim_end() == "---"
}

fn literal_inline_marker_prefix(value: &str) -> Option<&'static str> {
    literal_inline_tag_prefix(value).or_else(|| {
        ["**", "~~", "`", "[", "_"]
            .into_iter()
            .find(|marker| value.starts_with(marker))
    })
}

fn literal_inline_tag_prefix(value: &str) -> Option<&'static str> {
    ["<br />", "<br/>", "<br>", "</u>", "<u>"]
        .into_iter()
        .find(|tag| value.starts_with(tag))
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
    format!(
        "| {} |",
        cells
            .iter()
            .map(|cell| markdown_table_cell(cell))
            .collect::<Vec<_>>()
            .join(" | ")
    )
}

fn markdown_table_cell(value: &str) -> String {
    value.replace('\\', "\\\\").replace('|', "\\|")
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
    if glyph_type.contains("DECIMAL")
        || glyph_type.contains("NUMBER")
        || glyph_type.contains("ALPHA")
        || glyph_type.contains("ROMAN")
        || glyph_type.contains("ORDERED")
        || glyph_type.contains("ZERO")
        || glyph_type.contains("DIGIT")
    {
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
