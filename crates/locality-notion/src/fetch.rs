//! Fetch full Notion page content from the paginated block API.

use locality_core::LocalityResult;

use crate::client::NotionApi;
use crate::dto::{BlockTreeDto, NotionPageBundle, PageDto};

pub fn fetch_page_bundle(api: &dyn NotionApi, page_id: &str) -> LocalityResult<NotionPageBundle> {
    let page = api.retrieve_page(page_id)?;
    fetch_known_page_bundle(api, page_id, page)
}

/// Fetches block content after the caller has already classified an object as a page.
pub(crate) fn fetch_known_page_bundle(
    api: &dyn NotionApi,
    page_id: &str,
    page: PageDto,
) -> LocalityResult<NotionPageBundle> {
    let blocks = fetch_block_trees(api, page_id)?;
    Ok(NotionPageBundle { page, blocks })
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
