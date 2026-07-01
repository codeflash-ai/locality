//! Fetch full Notion page content from the paginated block API.

use locality_core::LocalityError;
use locality_core::LocalityResult;

use crate::client::NotionApi;
use crate::dto::{
    BlockDto, BlockTreeDto, CommentAnchorKind, CommentDto, CommentThreadDto, NotionPageBundle,
};
use crate::render::rich_text_plain_text;

pub fn fetch_page_bundle(api: &dyn NotionApi, page_id: &str) -> LocalityResult<NotionPageBundle> {
    let page = api.retrieve_page(page_id)?;
    let blocks = fetch_block_trees(api, page_id)?;
    let comments = fetch_comment_threads(api, page_id, &blocks)?;

    Ok(NotionPageBundle {
        page,
        blocks,
        comments,
    })
}

fn fetch_block_trees(api: &dyn NotionApi, block_id: &str) -> LocalityResult<Vec<BlockTreeDto>> {
    let mut cursor = None;
    let mut trees = Vec::new();

    loop {
        let page = api.retrieve_block_children(block_id, cursor.as_deref())?;
        for block in page.results {
            let children = if should_fetch_children_for_render(&block.kind, block.has_children) {
                fetch_block_trees(api, &block.id)?
            } else {
                Vec::new()
            };
            trees.push(BlockTreeDto { block, children });
        }

        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
    }

    Ok(trees)
}

fn should_fetch_children_for_render(kind: &str, has_children: bool) -> bool {
    has_children && !matches!(kind, "child_page" | "child_database")
}

fn fetch_comment_threads(
    api: &dyn NotionApi,
    page_id: &str,
    blocks: &[BlockTreeDto],
) -> LocalityResult<Vec<CommentThreadDto>> {
    let mut threads = Vec::new();
    match fetch_comments(api, page_id) {
        Ok(comments) => {
            if !comments.is_empty() {
                threads.push(CommentThreadDto {
                    anchor_id: page_id.to_string(),
                    anchor_kind: CommentAnchorKind::Page,
                    comments,
                });
            }
        }
        Err(error) if is_comment_capability_error(&error) => {
            return Ok(vec![CommentThreadDto {
                anchor_id: page_id.to_string(),
                anchor_kind: CommentAnchorKind::Unavailable,
                comments: Vec::new(),
            }]);
        }
        Err(error) => return Err(error),
    }

    for block in rendered_comment_anchor_blocks(blocks) {
        match fetch_comments(api, &block.id) {
            Ok(comments) => {
                if !comments.is_empty() {
                    threads.push(CommentThreadDto {
                        anchor_id: block.id.clone(),
                        anchor_kind: CommentAnchorKind::Block {
                            block_kind: block.kind.clone(),
                            quote: block_anchor_quote(block),
                        },
                        comments,
                    });
                }
            }
            Err(error) if is_comment_capability_error(&error) => {
                return Ok(vec![CommentThreadDto {
                    anchor_id: page_id.to_string(),
                    anchor_kind: CommentAnchorKind::Unavailable,
                    comments: Vec::new(),
                }]);
            }
            Err(error) => return Err(error),
        }
    }

    Ok(threads)
}

fn fetch_comments(api: &dyn NotionApi, parent_id: &str) -> LocalityResult<Vec<CommentDto>> {
    let mut cursor = None;
    let mut comments = Vec::new();

    loop {
        let page = api.list_comments(parent_id, cursor.as_deref())?;
        comments.extend(page.results);
        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
    }

    Ok(comments)
}

fn rendered_comment_anchor_blocks(trees: &[BlockTreeDto]) -> Vec<&BlockDto> {
    let mut blocks = Vec::new();
    for tree in trees {
        if has_local_block_anchor(&tree.block) {
            blocks.push(&tree.block);
        }
        blocks.extend(rendered_comment_anchor_blocks(&tree.children));
    }
    blocks
}

fn has_local_block_anchor(block: &BlockDto) -> bool {
    !matches!(block.kind.as_str(), "table_row")
        && block_anchor_quote(block)
            .as_deref()
            .is_none_or(|quote| !quote.trim().is_empty())
}

fn block_anchor_quote(block: &BlockDto) -> Option<String> {
    let text = match block.kind.as_str() {
        "paragraph" => block
            .paragraph
            .as_ref()
            .map(|block| rich_text_plain_text(&block.rich_text)),
        "heading_1" => block
            .heading_1
            .as_ref()
            .map(|block| rich_text_plain_text(&block.rich_text)),
        "heading_2" => block
            .heading_2
            .as_ref()
            .map(|block| rich_text_plain_text(&block.rich_text)),
        "heading_3" => block
            .heading_3
            .as_ref()
            .map(|block| rich_text_plain_text(&block.rich_text)),
        "heading_4" => block
            .heading_4
            .as_ref()
            .map(|block| rich_text_plain_text(&block.rich_text)),
        "bulleted_list_item" => block
            .bulleted_list_item
            .as_ref()
            .map(|block| rich_text_plain_text(&block.rich_text)),
        "numbered_list_item" => block
            .numbered_list_item
            .as_ref()
            .map(|block| rich_text_plain_text(&block.rich_text)),
        "to_do" => block
            .to_do
            .as_ref()
            .map(|block| rich_text_plain_text(&block.rich_text)),
        "quote" => block
            .quote
            .as_ref()
            .map(|block| rich_text_plain_text(&block.rich_text)),
        "callout" => block
            .callout
            .as_ref()
            .map(|block| rich_text_plain_text(&block.rich_text)),
        "code" => block
            .code
            .as_ref()
            .map(|block| rich_text_plain_text(&block.rich_text)),
        "child_page" => block.child_page.as_ref().map(|child| child.title.clone()),
        _ => None,
    }?;
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        None
    } else {
        Some(text.chars().take(120).collect())
    }
}

fn is_comment_capability_error(error: &LocalityError) -> bool {
    match error {
        LocalityError::Io(message) => {
            message.contains("HTTP 403")
                || message.to_ascii_lowercase().contains("comment capability")
        }
        LocalityError::Unsupported(feature) => feature.contains("comment"),
        LocalityError::NotImplemented(feature) => feature.contains("comment"),
        _ => false,
    }
}
