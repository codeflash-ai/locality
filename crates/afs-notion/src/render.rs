//! Render Notion page bundles to AgentFS canonical Markdown and shadows.

use afs_connector::NativeEntity;
use afs_core::model::{CanonicalDocument, RemoteId};
use afs_core::shadow::{MarkdownBlockKind, ShadowDocument};
use afs_core::{AfsError, AfsResult};

use crate::dto::{
    BlockDto, BlockTreeDto, NotionPageBundle, PageDto, RichTextBlockDto, RichTextDto,
    TableBlockDto, TableRowBlockDto,
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
    let frontmatter = frontmatter(&bundle.page, &title);
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

fn frontmatter(page: &PageDto, title: &str) -> String {
    format!(
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
    )
}

pub(crate) fn page_title(page: &PageDto) -> String {
    page.properties
        .values()
        .find(|property| property.kind == "title")
        .map(|property| rich_text_plain_text(&property.title))
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "Untitled".to_string())
}

fn rich_text_plain_text(rich_text: &[RichTextDto]) -> String {
    rich_text
        .iter()
        .map(|part| part.plain_text.as_str())
        .collect::<String>()
}

fn rich_text_to_markdown(rich_text: &[RichTextDto]) -> String {
    rich_text
        .iter()
        .map(|part| {
            let mut text = escape_markdown_text(&part.plain_text);
            if part.annotations.code {
                text = format!("`{}`", text.replace('`', "\\`"));
            }
            if part.annotations.bold {
                text = format!("**{text}**");
            }
            if part.annotations.italic {
                text = format!("_{text}_");
            }
            if part.annotations.strikethrough {
                text = format!("~~{text}~~");
            }
            if let Some(href) = &part.href {
                text = format!("[{text}]({href})");
            }
            text
        })
        .collect::<String>()
}

fn escape_markdown_text(text: &str) -> String {
    text.replace('\\', "\\\\")
}

fn escape_directive_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn yaml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
