//! Render Notion page bundles to Locality canonical Markdown and shadows.

use std::collections::BTreeSet;
use std::path::PathBuf;

use locality_connector::NativeEntity;
use locality_core::model::{CanonicalDocument, RemoteId};
use locality_core::shadow::{MarkdownBlockKind, ShadowDocument};
use locality_core::{LocalityError, LocalityResult};

use crate::dto::{
    BlockDto, BlockTreeDto, DateMentionDto, EquationBlockDto, FileBlockDto, LinkToPageBlockDto,
    MeetingNotesBlockDto, NotionPageBundle, PageDto, PagePropertyDto, RichTextBlockDto,
    RichTextDto, SyncedBlockDto, TableBlockDto, TableRowBlockDto, UrlBlockDto,
};
use crate::media::{MediaAsset, is_downloadable_url, local_media_href, media_local_path};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotionRenderedEntity {
    pub document: CanonicalDocument,
    pub shadow: ShadowDocument,
    pub remote_edited_at: Option<String>,
    pub media_assets: Vec<MediaAsset>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderOptions {
    page_path: Option<PathBuf>,
    local_media_block_ids: Option<BTreeSet<String>>,
}

impl RenderOptions {
    pub fn with_page_path(page_path: impl Into<PathBuf>) -> Self {
        Self {
            page_path: Some(page_path.into()),
            local_media_block_ids: None,
        }
    }

    pub fn with_local_media_block_ids(
        mut self,
        block_ids: impl IntoIterator<Item = String>,
    ) -> Self {
        self.local_media_block_ids = Some(block_ids.into_iter().collect());
        self
    }

    fn use_local_media_for(&self, block_id: &str) -> bool {
        match &self.local_media_block_ids {
            Some(block_ids) => block_ids.contains(block_id),
            None => true,
        }
    }
}

pub fn render_native_entity(entity: &NativeEntity) -> LocalityResult<NotionRenderedEntity> {
    render_native_entity_with_options(entity, &RenderOptions::default())
}

pub fn render_native_entity_with_options(
    entity: &NativeEntity,
    options: &RenderOptions,
) -> LocalityResult<NotionRenderedEntity> {
    let bundle = serde_json::from_slice::<NotionPageBundle>(&entity.raw)
        .map_err(|error| LocalityError::Io(format!("notion native decode failed: {error}")))?;
    render_page_bundle_with_options(&bundle, options)
}

pub fn render_page_bundle(bundle: &NotionPageBundle) -> LocalityResult<NotionRenderedEntity> {
    render_page_bundle_with_options(bundle, &RenderOptions::default())
}

pub fn render_page_bundle_with_options(
    bundle: &NotionPageBundle,
    options: &RenderOptions,
) -> LocalityResult<NotionRenderedEntity> {
    let title = page_title(&bundle.page);
    let frontmatter = page_frontmatter(&bundle.page, &title);
    let mut rendered_blocks = Vec::new();
    render_block_trees(&bundle.blocks, options, &mut rendered_blocks);

    let body = render_markdown_body(&rendered_blocks);
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
    .map_err(|error| LocalityError::InvalidState(format!("notion shadow build failed: {error}")))?
    .with_frontmatter(frontmatter.clone());
    apply_shadow_metadata(&mut shadow, &rendered_blocks);

    Ok(NotionRenderedEntity {
        document: CanonicalDocument::new(frontmatter, body),
        shadow,
        remote_edited_at: bundle.page.last_edited_time.clone(),
        media_assets: rendered_blocks
            .into_iter()
            .filter_map(|block| block.media_asset)
            .collect(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderedBlock {
    markdown: String,
    shadow_id: Option<RemoteId>,
    native_kind: Option<String>,
    metadata: RenderedBlockMetadata,
    spacing: RenderedBlockSpacing,
    media_asset: Option<MediaAsset>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RenderedBlockSpacing {
    #[default]
    Normal,
    BulletedListItem,
    NumberedListItem,
    Omitted,
}

impl RenderedBlockSpacing {
    fn is_list_item(self) -> bool {
        matches!(
            self,
            RenderedBlockSpacing::BulletedListItem | RenderedBlockSpacing::NumberedListItem
        )
    }
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

fn render_markdown_body(blocks: &[RenderedBlock]) -> String {
    let mut body = String::new();
    let mut previous_non_empty: Option<&RenderedBlock> = None;
    let mut pending_empty_blocks = 0;
    let mut numbered_list_index = 0;

    for block in blocks {
        if block.markdown.is_empty() {
            if block.spacing != RenderedBlockSpacing::Omitted {
                pending_empty_blocks += 1;
            }
            continue;
        }

        let continues_numbered_list = previous_non_empty.is_some_and(|previous| {
            pending_empty_blocks == 0 && previous.spacing == RenderedBlockSpacing::NumberedListItem
        });

        if let Some(previous) = previous_non_empty {
            if pending_empty_blocks == 0 && should_tight_join(previous, block) {
                body.push('\n');
            } else if pending_empty_blocks == 0 {
                body.push_str("\n\n");
            } else {
                body.push_str(&"\n".repeat(pending_empty_blocks + 1));
            }
        }
        if block.spacing == RenderedBlockSpacing::NumberedListItem {
            if continues_numbered_list {
                numbered_list_index += 1;
            } else {
                numbered_list_index = 1;
            }
            body.push_str(&renumbered_numbered_list_item(
                &block.markdown,
                numbered_list_index,
            ));
        } else {
            numbered_list_index = 0;
            body.push_str(&block.markdown);
        }
        previous_non_empty = Some(block);
        pending_empty_blocks = 0;
    }

    if body.is_empty() {
        String::new()
    } else {
        format!("{body}\n")
    }
}

fn should_tight_join(previous: &RenderedBlock, next: &RenderedBlock) -> bool {
    previous.spacing.is_list_item() && next.spacing.is_list_item()
}

fn renumbered_numbered_list_item(markdown: &str, ordinal: usize) -> String {
    let marker_start = markdown
        .char_indices()
        .find_map(|(index, ch)| (ch != ' ').then_some(index))
        .unwrap_or(markdown.len());
    let (indent, rest) = markdown.split_at(marker_start);

    rest.strip_prefix("1. ")
        .map(|text| format!("{indent}{ordinal}. {text}"))
        .unwrap_or_else(|| markdown.to_string())
}

fn render_block_trees(
    trees: &[BlockTreeDto],
    options: &RenderOptions,
    out: &mut Vec<RenderedBlock>,
) {
    render_block_trees_with_indent(trees, options, out, 0);
}

fn render_block_trees_with_indent(
    trees: &[BlockTreeDto],
    options: &RenderOptions,
    out: &mut Vec<RenderedBlock>,
    indent_level: usize,
) {
    for tree in trees {
        if tree.block.kind == "table"
            && let Some(rendered) = render_table_tree(tree)
        {
            out.push(indent_rendered_block(rendered, indent_level));
            continue;
        }

        out.push(indent_rendered_block(
            render_block(&tree.block, options),
            indent_level,
        ));
        let child_indent_level = if tree.block.kind == "toggle" {
            indent_level + 1
        } else {
            indent_level
        };
        render_block_trees_with_indent(&tree.children, options, out, child_indent_level);
    }
}

fn render_block(block: &BlockDto, options: &RenderOptions) -> RenderedBlock {
    let shadow_id = Some(RemoteId::new(block.id.clone()));
    let mut rendered = match block.kind.as_str() {
        "paragraph" => paragraph_block(block, block.paragraph.as_ref()),
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
        "heading_4" => rich_text_block(
            block,
            block.heading_4.as_ref(),
            |text| format!("#### {text}"),
            "heading_4",
        ),
        "bulleted_list_item" => list_item_block(
            block,
            block.bulleted_list_item.as_ref(),
            |text| format!("- {text}"),
            RenderedBlockSpacing::BulletedListItem,
        ),
        "numbered_list_item" => list_item_block(
            block,
            block.numbered_list_item.as_ref(),
            |text| format!("1. {text}"),
            RenderedBlockSpacing::NumberedListItem,
        ),
        "to_do" => match &block.to_do {
            Some(to_do) => {
                let text = rich_text_to_markdown(&to_do.rich_text);
                let marker = if to_do.checked { "x" } else { " " };
                if text.trim().is_empty() {
                    directive_block(block, "empty_to_do", None)
                } else {
                    list_item_rendered_block(
                        format!("- [{marker}] {text}"),
                        shadow_id,
                        RenderedBlockSpacing::BulletedListItem,
                    )
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
                rendered_block(
                    code_fence_markdown(language, &rich_text_plain_text(&code.rich_text)),
                    shadow_id,
                )
            }
            None => directive_block(block, "malformed_code", None),
        },
        "divider" => rendered_block("---".to_string(), shadow_id),
        "child_page" => child_page_link(block),
        "child_database" => directive_block(
            block,
            "child_database",
            block
                .child_database
                .as_ref()
                .map(|child| child.title.as_str()),
        ),
        "toggle" => rich_text_block(
            block,
            block.toggle.as_ref(),
            |text| format!("- {text}"),
            "toggle",
        ),
        "equation" => equation_block(block, block.equation.as_ref()),
        "embed" => url_markdown_block(block, "embed", block.embed.as_ref()),
        "bookmark" => url_markdown_block(block, "bookmark", block.bookmark.as_ref()),
        "link_preview" => url_markdown_block(block, "link_preview", block.link_preview.as_ref()),
        "image" => file_media_block(block, "image", block.image.as_ref(), options),
        "video" => file_media_block(block, "video", block.video.as_ref(), options),
        "file" => file_media_block(block, "file", block.file.as_ref(), options),
        "pdf" => file_media_block(block, "pdf", block.pdf.as_ref(), options),
        "audio" => file_media_block(block, "audio", block.audio.as_ref(), options),
        "synced_block" => synced_block_directive(block, block.synced_block.as_ref()),
        "link_to_page" => link_to_page_directive(block, block.link_to_page.as_ref()),
        "table_of_contents" => directive_block_with_attrs(
            block,
            "table_of_contents",
            block
                .table_of_contents
                .as_ref()
                .and_then(|table| table.color.clone())
                .map(|color| vec![("color", color)])
                .unwrap_or_default(),
        ),
        "breadcrumb" | "column_list" | "column" => directive_block(block, &block.kind, None),
        "template" => directive_block(
            block,
            "template",
            block
                .template
                .as_ref()
                .and_then(rich_text_block_title)
                .as_deref(),
        ),
        "meeting_notes" => titled_directive(block, "meeting_notes", block.meeting_notes.as_ref()),
        "transcription" => titled_directive(block, "transcription", block.transcription.as_ref()),
        "tab" | "ai_block" | "custom_block" | "button" => directive_block(block, &block.kind, None),
        "unsupported" => unsupported_block(block),
        other => directive_block(block, &format!("unsupported_{other}"), None),
    };
    if rendered.shadow_id.is_some() {
        rendered.native_kind = Some(block.kind.clone());
    }
    rendered
}

fn paragraph_block(block: &BlockDto, content: Option<&RichTextBlockDto>) -> RenderedBlock {
    let Some(content) = content else {
        return directive_block(block, "malformed_paragraph", None);
    };
    let text = paragraph_text_to_markdown(&content.rich_text);

    if text.trim().is_empty() {
        // empty line
        rendered_block(String::new(), None)
    } else {
        rendered_block(text, Some(RemoteId::new(block.id.clone())))
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
        rendered_block(render(&text), Some(RemoteId::new(block.id.clone())))
    }
}

fn list_item_block(
    block: &BlockDto,
    content: Option<&RichTextBlockDto>,
    render: impl FnOnce(&str) -> String,
    spacing: RenderedBlockSpacing,
) -> RenderedBlock {
    let Some(content) = content else {
        return directive_block(block, &format!("malformed_{}", block.kind), None);
    };
    let text = rich_text_to_markdown(&content.rich_text);
    let markdown = if text.trim().is_empty() && spacing == RenderedBlockSpacing::BulletedListItem {
        "-".to_string()
    } else {
        render(&text)
    };
    list_item_rendered_block(markdown, Some(RemoteId::new(block.id.clone())), spacing)
}

fn equation_block(block: &BlockDto, equation: Option<&EquationBlockDto>) -> RenderedBlock {
    let Some(expression) = equation
        .map(|equation| equation.expression.trim())
        .filter(|expression| !expression.is_empty())
    else {
        return directive_block(block, "malformed_equation", None);
    };

    rendered_block(
        format!("$$\n{expression}\n$$"),
        Some(RemoteId::new(block.id.clone())),
    )
}

fn url_markdown_block(
    block: &BlockDto,
    malformed_type: &'static str,
    payload: Option<&UrlBlockDto>,
) -> RenderedBlock {
    let Some(payload) = payload else {
        return directive_block(block, &format!("malformed_{malformed_type}"), None);
    };
    if payload.url.trim().is_empty() {
        return directive_block(block, &format!("malformed_{malformed_type}"), None);
    }

    let label = rich_text_list_title(&payload.caption)
        .filter(|caption| !caption.trim().is_empty())
        .unwrap_or_else(|| payload.url.clone());
    rendered_block(
        markdown_link_preserving_whitespace(&label, &payload.url),
        Some(RemoteId::new(block.id.clone())),
    )
}

fn file_media_block(
    block: &BlockDto,
    media_type: &'static str,
    payload: Option<&FileBlockDto>,
    options: &RenderOptions,
) -> RenderedBlock {
    let mut attrs = Vec::new();
    let mut media_asset = None;

    if let Some(payload) = payload {
        let title = rich_text_list_title(&payload.caption);
        if let Some(title) = title.clone() {
            attrs.push(("title", title));
        }
        if let Some(url) = file_url(payload) {
            let mut markdown_url = url.clone();
            if is_downloadable_url(&url)
                && let Some(page_path) = options.page_path.as_deref()
            {
                let local_path = media_local_path(page_path, &block.id, media_type, &url);
                if options.use_local_media_for(&block.id) {
                    markdown_url = local_media_href(page_path, &local_path);
                }
                media_asset = Some(MediaAsset {
                    block_id: block.id.clone(),
                    kind: media_type.to_string(),
                    source_url: url.clone(),
                    local_path,
                });
            }

            let label = title.unwrap_or_else(|| media_default_label(media_type).to_string());
            let markdown_url = escape_markdown_link_href(&markdown_url);
            let markdown = if media_type == "image" {
                format!("![{}]({markdown_url})", escape_markdown_link_label(&label))
            } else {
                format!("[{}]({markdown_url})", escape_markdown_link_label(&label))
            };
            let mut rendered = rendered_block(markdown, Some(RemoteId::new(block.id.clone())));
            rendered.media_asset = media_asset;
            return rendered;
        }
    }

    let mut rendered = directive_block_with_attrs(block, media_type, attrs);
    rendered.media_asset = media_asset;
    rendered
}

fn media_default_label(media_type: &str) -> &'static str {
    match media_type {
        "image" => "Image",
        "video" => "Video",
        "file" => "File",
        "pdf" => "PDF",
        "audio" => "Audio",
        _ => "Media",
    }
}

fn synced_block_directive(block: &BlockDto, payload: Option<&SyncedBlockDto>) -> RenderedBlock {
    let attrs = payload
        .and_then(|payload| payload.synced_from.as_ref())
        .and_then(|synced_from| synced_from.block_id.clone())
        .map(|block_id| vec![("source_block_id", block_id)])
        .unwrap_or_default();
    directive_block_with_attrs(block, "synced_block", attrs)
}

fn link_to_page_directive(block: &BlockDto, payload: Option<&LinkToPageBlockDto>) -> RenderedBlock {
    let Some(payload) = payload else {
        return directive_block(block, "malformed_link_to_page", None);
    };

    let link = match payload.kind.as_str() {
        "page_id" => payload
            .page_id
            .as_deref()
            .map(|id| ("Linked page", notion_object_url(id))),
        "database_id" => payload
            .database_id
            .as_deref()
            .map(|id| ("Linked database", notion_object_url(id))),
        _ => None,
    };

    match link {
        Some((label, href)) => rendered_block(
            markdown_link_preserving_whitespace(label, &href),
            Some(RemoteId::new(block.id.clone())),
        ),
        None => directive_block(block, "malformed_link_to_page", None),
    }
}

fn child_page_link(block: &BlockDto) -> RenderedBlock {
    let title = block
        .child_page
        .as_ref()
        .map(|child| child.title.as_str())
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("Untitled child page");
    rendered_block(
        markdown_link_preserving_whitespace(title, &notion_object_url(&block.id)),
        Some(RemoteId::new(block.id.clone())),
    )
}

fn titled_directive(
    block: &BlockDto,
    directive_type: &str,
    payload: Option<&MeetingNotesBlockDto>,
) -> RenderedBlock {
    directive_block(
        block,
        directive_type,
        payload.and_then(|meeting_notes| meeting_notes.title.as_deref()),
    )
}

fn directive_block(block: &BlockDto, directive_type: &str, title: Option<&str>) -> RenderedBlock {
    let attrs = title
        .map(|title| vec![("title", title.to_string())])
        .unwrap_or_default();
    directive_block_with_attrs(block, directive_type, attrs)
}

fn directive_block_with_attrs(
    block: &BlockDto,
    directive_type: &str,
    attrs: Vec<(&'static str, String)>,
) -> RenderedBlock {
    let mut parts = vec![format!("id={}", block.id), format!("type={directive_type}")];
    for (key, value) in attrs {
        if !value.is_empty() {
            parts.push(format!("{key}=\"{}\"", escape_directive_value(&value)));
        }
    }
    let markdown = format!("::loc{{{}}}", parts.join(" "));
    RenderedBlock {
        markdown,
        shadow_id: None,
        native_kind: None,
        metadata: RenderedBlockMetadata::None,
        spacing: RenderedBlockSpacing::Normal,
        media_asset: None,
    }
}

fn unsupported_block(block: &BlockDto) -> RenderedBlock {
    let Some(block_type) = block.unsupported.as_ref().and_then(|value| {
        value
            .block_type
            .as_deref()
            .filter(|block_type| !block_type.is_empty())
    }) else {
        return directive_block(block, "unsupported", Some("Unsupported Notion block"));
    };

    if is_subtype_only_unsupported_artifact(block_type) {
        return omitted_block();
    }

    directive_block_with_attrs(
        block,
        "unsupported",
        vec![("block_type", block_type.to_string())],
    )
}

fn is_subtype_only_unsupported_artifact(block_type: &str) -> bool {
    matches!(block_type, "alias" | "button" | "copy_indicator")
}

fn rendered_block(markdown: String, shadow_id: Option<RemoteId>) -> RenderedBlock {
    RenderedBlock {
        markdown,
        shadow_id,
        native_kind: None,
        metadata: RenderedBlockMetadata::None,
        spacing: RenderedBlockSpacing::Normal,
        media_asset: None,
    }
}

fn omitted_block() -> RenderedBlock {
    let mut block = rendered_block(String::new(), None);
    block.spacing = RenderedBlockSpacing::Omitted;
    block
}

fn list_item_rendered_block(
    markdown: String,
    shadow_id: Option<RemoteId>,
    spacing: RenderedBlockSpacing,
) -> RenderedBlock {
    let mut block = rendered_block(markdown, shadow_id);
    block.spacing = spacing;
    block
}

fn code_fence_markdown(language: &str, code: &str) -> String {
    let fence = "`".repeat(max_consecutive_backticks(code).saturating_add(1).max(3));
    format!("{fence}{language}\n{code}\n{fence}")
}

fn max_consecutive_backticks(value: &str) -> usize {
    let mut max_run = 0;
    let mut current = 0;
    for ch in value.chars() {
        if ch == '`' {
            current += 1;
            max_run = max_run.max(current);
        } else {
            current = 0;
        }
    }
    max_run
}

fn indent_rendered_block(mut block: RenderedBlock, indent_level: usize) -> RenderedBlock {
    if indent_level == 0 {
        return block;
    }

    let prefix = "    ".repeat(indent_level);
    block.markdown = block
        .markdown
        .lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    block
}

fn rich_text_block_title(block: &RichTextBlockDto) -> Option<String> {
    rich_text_list_title(&block.rich_text)
}

fn rich_text_list_title(rich_text: &[RichTextDto]) -> Option<String> {
    let title = rich_text_plain_text(rich_text);
    if title.trim().is_empty() {
        None
    } else {
        Some(title)
    }
}

fn file_url(file: &FileBlockDto) -> Option<String> {
    file.external
        .as_ref()
        .map(|external| external.url.clone())
        .or_else(|| file.file.as_ref().map(|file| file.url.clone()))
        .filter(|url| !url.is_empty())
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
        native_kind: Some(tree.block.kind.clone()),
        metadata: RenderedBlockMetadata::Table {
            row_ids,
            has_column_header: table.has_column_header,
            has_row_header: table.has_row_header,
        },
        spacing: RenderedBlockSpacing::Normal,
        media_asset: None,
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
    for (shadow_block, rendered_block) in shadow.blocks.iter_mut().zip(
        rendered_blocks
            .iter()
            .filter(|block| !block.markdown.trim().is_empty()),
    ) {
        shadow_block.native_kind = rendered_block.native_kind.clone();
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
        "loc:\n  id: {}\n  type: page\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\n",
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

fn paragraph_text_to_markdown(rich_text: &[RichTextDto]) -> String {
    escape_paragraph_block_start_marker(rich_text_to_markdown(rich_text))
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
    escape_markdown_text_with_options(&rich_text_part_plain_text(part), !part.annotations.code)
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
                        &notion_object_url(&page.id),
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
                        &notion_object_url(&database.id),
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
        format!(
            "[{}]({})",
            escape_markdown_link_label(value),
            escape_markdown_link_href(href)
        )
    })
}

fn notion_object_url(id: &str) -> String {
    format!("https://www.notion.so/{}", notion_url_id(id))
}

fn notion_url_id(id: &str) -> String {
    let hex = id
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>();
    if hex.len() == 32 { hex } else { id.to_string() }
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
    escape_markdown_text_with_options(text, true)
}

fn escape_markdown_text_with_options(text: &str, escape_inline_markers: bool) -> String {
    let mut escaped = String::with_capacity(text.len());
    let mut rest = text;

    while !rest.is_empty() {
        if escape_inline_markers && let Some(marker) = literal_inline_marker_prefix(rest) {
            escaped.push('\\');
            escaped.push_str(marker);
            rest = &rest[marker.len()..];
            continue;
        }

        let ch = rest.chars().next().expect("non-empty rest");
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '$' if escape_inline_markers => escaped.push_str("\\$"),
            '\n' => escaped.push_str("<br>"),
            _ => escaped.push(ch),
        }
        rest = &rest[ch.len_utf8()..];
    }

    escaped
}

fn escape_markdown_link_label(text: &str) -> String {
    escape_markdown_text(text).replace(']', "\\]")
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
        || list_marker(value)
        || quote_marker(value)
        || divider_marker(value)
}

fn heading_marker(value: &str) -> bool {
    let level = value.chars().take_while(|ch| *ch == '#').count();
    (1..=6).contains(&level) && value[level..].starts_with(char::is_whitespace)
}

fn list_marker(value: &str) -> bool {
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

fn break_tag_prefix(value: &str) -> Option<&'static str> {
    ["<br />", "<br/>", "<br>"]
        .into_iter()
        .find(|tag| value.starts_with(tag))
}

fn literal_inline_marker_prefix(value: &str) -> Option<&'static str> {
    literal_inline_tag_prefix(value).or_else(|| {
        [
            "**",
            "~~",
            "`",
            "[",
            "_",
            "@date(",
            "@page(",
            "@database(",
            "@user(",
        ]
        .into_iter()
        .find(|marker| value.starts_with(marker))
    })
}

fn literal_inline_tag_prefix(value: &str) -> Option<&'static str> {
    break_tag_prefix(value).or_else(|| {
        ["</u>", "<u>"]
            .into_iter()
            .find(|tag| value.starts_with(tag))
    })
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
            &rich_text_to_markdown(&property.rich_text),
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
        "files" => Some(FrontmatterValue::List(
            property.files.iter().map(file_property_label).collect(),
        )),
        "people" => Some(FrontmatterValue::List(
            property.people.iter().map(user_property_label).collect(),
        )),
        "relation" => Some(FrontmatterValue::List(
            property
                .relation
                .iter()
                .map(|relation| relation.id.clone())
                .collect(),
        )),
        "created_time" | "last_edited_time" | "created_by" | "last_edited_by" | "formula"
        | "rollup" | "unique_id" | "verification" => None,
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

fn file_property_label(file: &crate::dto::FilePropertyDto) -> String {
    let url = file
        .external
        .as_ref()
        .map(|external| external.url.as_str())
        .or_else(|| file.file.as_ref().map(|hosted| hosted.url.as_str()))
        .unwrap_or_default();
    let name = file.name.as_deref().unwrap_or_default();

    match (name.is_empty(), url.is_empty()) {
        (false, false) => format!("{name} <{url}>"),
        (false, true) => name.to_string(),
        (true, false) => url.to_string(),
        (true, true) => String::new(),
    }
}

fn user_property_label(user: &crate::dto::UserMentionDto) -> String {
    let id = user.id.as_str();
    let name = user.name.as_deref().unwrap_or_default();
    match (name.is_empty(), id.is_empty()) {
        (false, false) => format!("{name} <{id}>"),
        (false, true) => name.to_string(),
        (true, false) => id.to_string(),
        (true, true) => String::new(),
    }
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
