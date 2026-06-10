//! Render Notion page bundles to AgentFS canonical Markdown and shadows.

use afs_connector::NativeEntity;
use afs_core::model::{CanonicalDocument, RemoteId};
use afs_core::shadow::{MarkdownBlockKind, ShadowDocument};
use afs_core::{AfsError, AfsResult};
use serde_json::Value;

use crate::dto::{
    BlockDto, BlockTreeDto, DateMentionDto, NotionPageBundle, PageDto, PagePropertyDto,
    RichTextBlockDto, RichTextDto, TableBlockDto, TableRowBlockDto,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotionRenderedEntity {
    pub document: CanonicalDocument,
    pub shadow: ShadowDocument,
}

pub fn render_native_entity(entity: &NativeEntity) -> AfsResult<NotionRenderedEntity> {
    let bundle = serde_json::from_slice::<NotionPageBundle>(&entity.raw)
        .map_err(|error| AfsError::Io(format!("notion native decode failed: {error}")))?;
    render_page_bundle(&bundle)
}

pub fn render_page_bundle(bundle: &NotionPageBundle) -> AfsResult<NotionRenderedEntity> {
    let title = page_title(&bundle.page);
    let frontmatter = page_frontmatter(&bundle.page, &title);
    let mut rendered_blocks = Vec::new();
    render_block_trees(&bundle.blocks, &mut rendered_blocks);

    let body = rendered_blocks
        .iter()
        .map(|block| block.markdown.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let body = if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    };
    let shadow_ids = rendered_blocks
        .iter()
        .filter_map(|block| block.shadow_id.clone())
        .collect::<Vec<_>>();
    let body_start_line = frontmatter.lines().count() + 3;
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new(bundle.page.id.clone()),
        body.clone(),
        body_start_line,
        shadow_ids,
    )
    .map_err(|error| AfsError::InvalidState(format!("notion shadow build failed: {error}")))?;
    apply_shadow_metadata(&mut shadow, &rendered_blocks);

    Ok(NotionRenderedEntity {
        document: CanonicalDocument::new(frontmatter, body),
        shadow,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderedBlock {
    markdown: String,
    shadow_id: Option<RemoteId>,
    metadata: RenderedBlockMetadata,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum RenderedBlockMetadata {
    #[default]
    None,
    Table {
        row_ids: Vec<RemoteId>,
        has_column_header: bool,
        has_row_header: bool,
    },
}

fn render_block_trees(trees: &[BlockTreeDto], out: &mut Vec<RenderedBlock>) {
    for tree in trees {
        if tree.block.kind == "table"
            && let Some(rendered) = render_table_tree(tree)
        {
            out.push(rendered);
            continue;
        }

        out.push(render_block(&tree.block));
        render_block_trees(&tree.children, out);
    }
}

fn render_block(block: &BlockDto) -> RenderedBlock {
    let shadow_id = Some(RemoteId::new(block.id.clone()));
    match block.kind.as_str() {
        "paragraph" => rich_text_block(
            block,
            block.paragraph.as_ref(),
            |text| text.to_string(),
            "empty_paragraph",
        ),
        "heading_1" => rich_text_block(
            block,
            block.heading_1.as_ref(),
            |text| format!("# {text}"),
            "heading_1",
        ),
        "heading_2" => rich_text_block(
            block,
            block.heading_2.as_ref(),
            |text| format!("## {text}"),
            "heading_2",
        ),
        "heading_3" => rich_text_block(
            block,
            block.heading_3.as_ref(),
            |text| format!("### {text}"),
            "heading_3",
        ),
        "bulleted_list_item" => rich_text_block(
            block,
            block.bulleted_list_item.as_ref(),
            |text| format!("- {text}"),
            "bulleted_list_item",
        ),
        "numbered_list_item" => rich_text_block(
            block,
            block.numbered_list_item.as_ref(),
            |text| format!("1. {text}"),
            "numbered_list_item",
        ),
        "to_do" => match &block.to_do {
            Some(to_do) => {
                let text = rich_text_to_markdown(&to_do.rich_text);
                let marker = if to_do.checked { "x" } else { " " };
                if text.trim().is_empty() {
                    directive_block(block, "empty_to_do", None)
                } else {
                    RenderedBlock {
                        markdown: format!("- [{marker}] {text}"),
                        shadow_id,
                        metadata: RenderedBlockMetadata::None,
                    }
                }
            }
            None => directive_block(block, "malformed_to_do", None),
        },
        "quote" => rich_text_block(
            block,
            block.quote.as_ref(),
            |text| format!("> {text}"),
            "quote",
        ),
        "callout" => rich_text_block(
            block,
            block.callout.as_ref(),
            |text| format!("> [!NOTE]\n> {text}"),
            "callout",
        ),
        "code" => match &block.code {
            Some(code) => {
                let language = code.language.as_deref().unwrap_or_default();
                RenderedBlock {
                    markdown: format!(
                        "```{}\n{}\n```",
                        language,
                        rich_text_plain_text(&code.rich_text)
                    ),
                    shadow_id,
                    metadata: RenderedBlockMetadata::None,
                }
            }
            None => directive_block(block, "malformed_code", None),
        },
        "divider" => RenderedBlock {
            markdown: "---".to_string(),
            shadow_id,
            metadata: RenderedBlockMetadata::None,
        },
        "child_page" => directive_block(
            block,
            "child_page",
            block.child_page.as_ref().map(|child| child.title.as_str()),
        ),
        "child_database" => directive_block(
            block,
            "child_database",
            block
                .child_database
                .as_ref()
                .map(|child| child.title.as_str()),
        ),
        other => directive_block(block, &format!("unsupported_{other}"), None),
    }
}

fn rich_text_block(
    block: &BlockDto,
    content: Option<&RichTextBlockDto>,
    render: impl FnOnce(&str) -> String,
    empty_directive_type: &str,
) -> RenderedBlock {
    let Some(content) = content else {
        return directive_block(block, &format!("malformed_{}", block.kind), None);
    };
    let text = rich_text_to_markdown(&content.rich_text);

    if text.trim().is_empty() {
        directive_block(block, empty_directive_type, None)
    } else {
        RenderedBlock {
            markdown: render(&text),
            shadow_id: Some(RemoteId::new(block.id.clone())),
            metadata: RenderedBlockMetadata::None,
        }
    }
}

fn directive_block(block: &BlockDto, directive_type: &str, title: Option<&str>) -> RenderedBlock {
    let title = title.map(escape_directive_value);
    let markdown = match title {
        Some(title) => format!(
            "::afs{{id={} type={} title=\"{}\"}}",
            block.id, directive_type, title
        ),
        None => format!("::afs{{id={} type={}}}", block.id, directive_type),
    };

    RenderedBlock {
        markdown,
        shadow_id: None,
        metadata: RenderedBlockMetadata::None,
    }
}

fn render_table_tree(tree: &BlockTreeDto) -> Option<RenderedBlock> {
    let table = tree.block.table.as_ref()?;
    let rows = table_rows(&tree.children)?;
    let markdown = table_to_markdown(table, &rows)?;
    let row_ids = tree
        .children
        .iter()
        .map(|row| RemoteId::new(row.block.id.clone()))
        .collect();

    Some(RenderedBlock {
        markdown,
        shadow_id: Some(RemoteId::new(tree.block.id.clone())),
        metadata: RenderedBlockMetadata::Table {
            row_ids,
            has_column_header: table.has_column_header,
            has_row_header: table.has_row_header,
        },
    })
}

fn table_rows(children: &[BlockTreeDto]) -> Option<Vec<&TableRowBlockDto>> {
    children
        .iter()
        .map(|child| {
            if child.block.kind != "table_row" || !child.children.is_empty() {
                return None;
            }

            child.block.table_row.as_ref()
        })
        .collect()
}

fn table_to_markdown(table: &TableBlockDto, rows: &[&TableRowBlockDto]) -> Option<String> {
    let width = usize::from(table.table_width);
    if width == 0 || rows.is_empty() || rows.iter().any(|row| row.cells.len() != width) {
        return None;
    }

    let mut rendered = Vec::with_capacity(rows.len() + 1);
    if table.has_column_header {
        rendered.push(markdown_table_row(&rows[0].cells));
        rendered.push(markdown_table_separator(width));
        rendered.extend(rows[1..].iter().map(|row| markdown_table_row(&row.cells)));
    } else {
        rendered.push(markdown_table_row(&vec![Vec::new(); width]));
        rendered.push(markdown_table_separator(width));
        rendered.extend(rows.iter().map(|row| markdown_table_row(&row.cells)));
    }

    Some(rendered.join("\n"))
}

fn markdown_table_row(cells: &[Vec<RichTextDto>]) -> String {
    format!(
        "| {} |",
        cells
            .iter()
            .map(|cell| escape_table_cell(&rich_text_to_markdown(cell)))
            .collect::<Vec<_>>()
            .join(" | ")
    )
}

fn markdown_table_separator(width: usize) -> String {
    format!("| {} |", vec!["---"; width].join(" | "))
}

fn escape_table_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', "<br>")
}

fn apply_shadow_metadata(shadow: &mut ShadowDocument, rendered_blocks: &[RenderedBlock]) {
    for (shadow_block, rendered_block) in shadow.blocks.iter_mut().zip(rendered_blocks) {
        if let RenderedBlockMetadata::Table {
            row_ids,
            has_column_header,
            has_row_header,
        } = &rendered_block.metadata
        {
            shadow_block.kind = MarkdownBlockKind::TableWithRows {
                row_ids: row_ids.clone(),
                has_column_header: *has_column_header,
                has_row_header: *has_row_header,
            };
        }
    }
}

pub(crate) fn page_frontmatter(page: &PageDto, title: &str) -> String {
    let mut out = format!(
        "afs:\n  id: {}\n  type: page\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\n",
        page.id,
        yaml_string(
            page.last_edited_time
                .as_deref()
                .or(page.created_time.as_deref())
                .unwrap_or("unknown")
        ),
        yaml_string(page.last_edited_time.as_deref().unwrap_or("unknown")),
        yaml_string(title)
    );
    append_property_frontmatter(&mut out, page);
    out
}

pub(crate) fn page_title(page: &PageDto) -> String {
    page.properties
        .values()
        .find(|property| property.kind == "title")
        .map(|property| rich_text_plain_text(&property.title))
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "Untitled".to_string())
}

pub(crate) fn rich_text_plain_text(rich_text: &[RichTextDto]) -> String {
    rich_text
        .iter()
        .map(rich_text_part_plain_text)
        .collect::<String>()
}

fn rich_text_to_markdown(rich_text: &[RichTextDto]) -> String {
    rich_text
        .iter()
        .map(rich_text_part_to_markdown)
        .collect::<String>()
}

fn rich_text_part_to_markdown(part: &RichTextDto) -> String {
    let (mut text, link_applied) = match part.kind.as_str() {
        "equation" => (equation_to_markdown(part), false),
        "mention" => mention_to_markdown(part),
        _ => (text_rich_text_to_markdown(part), false),
    };

    text = apply_annotations(text, &part.annotations);

    if !link_applied && let Some(href) = rich_text_href(part) {
        text = markdown_link_preserving_whitespace(&text, href);
    }

    text
}

fn text_rich_text_to_markdown(part: &RichTextDto) -> String {
    escape_markdown_text(&rich_text_part_plain_text(part))
}

fn equation_to_markdown(part: &RichTextDto) -> String {
    let expression = part
        .equation
        .as_ref()
        .map(|equation| equation.expression.as_str())
        .filter(|expression| !expression.is_empty())
        .unwrap_or(part.plain_text.as_str());

    if expression.is_empty() {
        String::new()
    } else {
        format!("${}$", expression.replace('$', "\\$"))
    }
}

fn mention_to_markdown(part: &RichTextDto) -> (String, bool) {
    let Some(mention) = &part.mention else {
        return (text_rich_text_to_markdown(part), false);
    };

    match mention.kind.as_str() {
        "page" => mention
            .page
            .as_ref()
            .map(|page| {
                (
                    markdown_link_preserving_whitespace(
                        &mention_label(part),
                        &format!("afs://{}", page.id),
                    ),
                    true,
                )
            })
            .unwrap_or_else(|| (text_rich_text_to_markdown(part), false)),
        "database" => mention
            .database
            .as_ref()
            .map(|database| {
                (
                    markdown_link_preserving_whitespace(
                        &mention_label(part),
                        &format!("afs://{}", database.id),
                    ),
                    true,
                )
            })
            .unwrap_or_else(|| (text_rich_text_to_markdown(part), false)),
        "user" => {
            let fallback = || {
                mention
                    .user
                    .as_ref()
                    .and_then(|user| user.name.clone())
                    .map(|name| escape_markdown_text(&format!("@{name}")))
                    .unwrap_or_default()
            };
            (text_or_fallback(part, fallback), false)
        }
        "date" => {
            let fallback = || {
                mention
                    .date
                    .as_ref()
                    .map(date_mention_label)
                    .map(|label| escape_markdown_text(&label))
                    .unwrap_or_default()
            };
            (text_or_fallback(part, fallback), false)
        }
        "link_preview" => mention
            .link_preview
            .as_ref()
            .filter(|link| !link.url.is_empty())
            .map(|link| {
                (
                    markdown_link_preserving_whitespace(&mention_label(part), &link.url),
                    true,
                )
            })
            .unwrap_or_else(|| (text_rich_text_to_markdown(part), false)),
        _ => (text_rich_text_to_markdown(part), false),
    }
}

fn text_or_fallback(part: &RichTextDto, fallback: impl FnOnce() -> String) -> String {
    let text = text_rich_text_to_markdown(part);
    if text.is_empty() { fallback() } else { text }
}

fn mention_label(part: &RichTextDto) -> String {
    let label = rich_text_part_plain_text(part);
    if label.is_empty() {
        "mention".to_string()
    } else {
        escape_markdown_text(&label)
    }
}

fn date_mention_label(date: &crate::dto::DateMentionDto) -> String {
    match &date.end {
        Some(end) if !end.is_empty() => format!("{} to {end}", date.start),
        _ => date.start.clone(),
    }
}

fn apply_annotations(mut text: String, annotations: &crate::dto::RichTextAnnotationsDto) -> String {
    if annotations.code {
        text =
            wrap_preserving_whitespace(&text, |value| format!("`{}`", value.replace('`', "\\`")));
    }
    if annotations.bold {
        text = wrap_preserving_whitespace(&text, |value| format!("**{value}**"));
    }
    if annotations.italic {
        text = wrap_preserving_whitespace(&text, |value| format!("_{value}_"));
    }
    if annotations.strikethrough {
        text = wrap_preserving_whitespace(&text, |value| format!("~~{value}~~"));
    }
    if annotations.underline {
        text = wrap_preserving_whitespace(&text, |value| format!("<u>{value}</u>"));
    }

    text
}

fn rich_text_href(part: &RichTextDto) -> Option<&str> {
    part.href
        .as_deref()
        .or_else(|| {
            part.text
                .as_ref()?
                .link
                .as_ref()
                .map(|link| link.url.as_str())
        })
        .filter(|href| !href.is_empty())
}

fn rich_text_part_plain_text(part: &RichTextDto) -> String {
    if !part.plain_text.is_empty() {
        return part.plain_text.clone();
    }

    match part.kind.as_str() {
        "text" | "" => part
            .text
            .as_ref()
            .map(|text| text.content.clone())
            .unwrap_or_default(),
        "equation" => part
            .equation
            .as_ref()
            .map(|equation| equation.expression.clone())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn markdown_link_preserving_whitespace(label: &str, href: &str) -> String {
    wrap_preserving_whitespace(label, |value| {
        format!("[{}]({href})", escape_markdown_link_label(value))
    })
}

fn wrap_preserving_whitespace(value: &str, wrap: impl FnOnce(&str) -> String) -> String {
    let Some(start) = value
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(index, _)| index)
    else {
        return value.to_string();
    };
    let end = value
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(index, ch)| index + ch.len_utf8())
        .unwrap_or(value.len());

    format!(
        "{}{}{}",
        &value[..start],
        wrap(&value[start..end]),
        &value[end..]
    )
}

fn escape_markdown_text(text: &str) -> String {
    text.replace('\\', "\\\\")
}

fn escape_markdown_link_label(text: &str) -> String {
    text.replace('[', "\\[").replace(']', "\\]")
}

fn escape_directive_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn yaml_string(value: &str) -> String {
    format!("\"{}\"", escape_yaml_string(value))
}

fn escape_yaml_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn append_property_frontmatter(out: &mut String, page: &PageDto) {
    for (name, property) in &page.properties {
        if property.kind == "title" {
            continue;
        }

        let Some(value) = property_frontmatter_value(property) else {
            continue;
        };
        write_frontmatter_value(out, name, value);
    }
}

fn property_frontmatter_value(property: &PagePropertyDto) -> Option<FrontmatterValue> {
    match property.kind.as_str() {
        "rich_text" => Some(FrontmatterValue::Scalar(yaml_string(
            &rich_text_plain_text(&property.rich_text),
        ))),
        "number" => Some(number_value(property.number.as_ref())),
        "select" => Some(option_name(property.select.as_ref())),
        "multi_select" => Some(FrontmatterValue::List(
            property
                .multi_select
                .iter()
                .map(|option| option.name.clone())
                .collect(),
        )),
        "status" => Some(option_name(property.status.as_ref())),
        "checkbox" => Some(
            property
                .checkbox
                .map(FrontmatterValue::Bool)
                .unwrap_or(FrontmatterValue::Null),
        ),
        "date" => Some(date_value(property.date.as_ref())),
        "url" => Some(optional_string(property.url.as_deref())),
        "email" => Some(optional_string(property.email.as_deref())),
        "phone_number" => Some(optional_string(property.phone_number.as_deref())),
        "people" => Some(FrontmatterValue::List(
            property
                .people
                .iter()
                .map(|user| user.name.as_deref().unwrap_or(user.id.as_str()).to_string())
                .collect(),
        )),
        "relation" => Some(FrontmatterValue::List(
            property
                .relation
                .iter()
                .map(|relation| relation.id.clone())
                .collect(),
        )),
        "created_time" => Some(optional_string(property.created_time.as_deref())),
        "last_edited_time" => Some(optional_string(property.last_edited_time.as_deref())),
        "created_by" => Some(optional_user(property.created_by.as_ref())),
        "last_edited_by" => Some(optional_user(property.last_edited_by.as_ref())),
        "formula" => property.formula.as_ref().and_then(formula_value),
        _ => None,
    }
}

fn number_value(number: Option<&serde_json::Number>) -> FrontmatterValue {
    number
        .map(|number| FrontmatterValue::Scalar(number.to_string()))
        .unwrap_or(FrontmatterValue::Null)
}

fn option_name(option: Option<&crate::dto::SelectOptionDto>) -> FrontmatterValue {
    option
        .map(|option| FrontmatterValue::Scalar(yaml_string(&option.name)))
        .unwrap_or(FrontmatterValue::Null)
}

fn date_value(date: Option<&DateMentionDto>) -> FrontmatterValue {
    let Some(date) = date else {
        return FrontmatterValue::Null;
    };

    if date.end.is_none() && date.time_zone.is_none() {
        return FrontmatterValue::Scalar(yaml_string(&date.start));
    }

    let mut fields = vec![("start".to_string(), yaml_string(&date.start))];
    if let Some(end) = date.end.as_deref() {
        fields.push(("end".to_string(), yaml_string(end)));
    }
    if let Some(time_zone) = date.time_zone.as_deref() {
        fields.push(("time_zone".to_string(), yaml_string(time_zone)));
    }
    FrontmatterValue::Map(fields)
}

fn optional_string(value: Option<&str>) -> FrontmatterValue {
    value
        .map(|value| FrontmatterValue::Scalar(yaml_string(value)))
        .unwrap_or(FrontmatterValue::Null)
}

fn optional_user(value: Option<&crate::dto::UserMentionDto>) -> FrontmatterValue {
    value
        .map(|user| {
            FrontmatterValue::Scalar(yaml_string(
                user.name.as_deref().unwrap_or(user.id.as_str()),
            ))
        })
        .unwrap_or(FrontmatterValue::Null)
}

fn formula_value(value: &Value) -> Option<FrontmatterValue> {
    let kind = value.get("type").and_then(Value::as_str)?;
    match kind {
        "string" => Some(optional_string(value.get("string").and_then(Value::as_str))),
        "number" => value
            .get("number")
            .and_then(Value::as_f64)
            .map(|number| FrontmatterValue::Scalar(number.to_string())),
        "boolean" => value
            .get("boolean")
            .and_then(Value::as_bool)
            .map(FrontmatterValue::Bool),
        "date" => value.get("date").map(json_date_value),
        _ => None,
    }
}

fn json_date_value(value: &Value) -> FrontmatterValue {
    let Some(start) = value.get("start").and_then(Value::as_str) else {
        return FrontmatterValue::Null;
    };
    let end = value.get("end").and_then(Value::as_str);
    let time_zone = value.get("time_zone").and_then(Value::as_str);
    if end.is_none() && time_zone.is_none() {
        return FrontmatterValue::Scalar(yaml_string(start));
    }

    let mut fields = vec![("start".to_string(), yaml_string(start))];
    if let Some(end) = end {
        fields.push(("end".to_string(), yaml_string(end)));
    }
    if let Some(time_zone) = time_zone {
        fields.push(("time_zone".to_string(), yaml_string(time_zone)));
    }
    FrontmatterValue::Map(fields)
}

fn write_frontmatter_value(out: &mut String, key: &str, value: FrontmatterValue) {
    let key = yaml_string(key);
    match value {
        FrontmatterValue::Null => out.push_str(&format!("{key}: null\n")),
        FrontmatterValue::Bool(value) => out.push_str(&format!("{key}: {value}\n")),
        FrontmatterValue::Scalar(value) => out.push_str(&format!("{key}: {value}\n")),
        FrontmatterValue::List(items) => {
            if items.is_empty() {
                out.push_str(&format!("{key}: []\n"));
            } else {
                out.push_str(&format!("{key}:\n"));
                for item in items {
                    out.push_str(&format!("  - {}\n", yaml_string(&item)));
                }
            }
        }
        FrontmatterValue::Map(fields) => {
            if fields.is_empty() {
                out.push_str(&format!("{key}: {{}}\n"));
            } else {
                out.push_str(&format!("{key}:\n"));
                for (field, value) in fields {
                    out.push_str(&format!("  {}: {value}\n", yaml_string(&field)));
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FrontmatterValue {
    Null,
    Bool(bool),
    Scalar(String),
    List(Vec<String>),
    Map(Vec<(String, String)>),
}
