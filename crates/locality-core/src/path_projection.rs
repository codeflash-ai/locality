//! Filesystem path helpers for projected page documents.

use std::path::{Path, PathBuf};

pub const PAGE_DOCUMENT_FILENAME: &str = "page.md";

pub fn page_document_path(page_directory: &Path) -> PathBuf {
    page_directory.join(PAGE_DOCUMENT_FILENAME)
}

pub fn is_page_document_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(PAGE_DOCUMENT_FILENAME))
}

pub fn page_container_path(page_path: &Path) -> PathBuf {
    if is_page_document_path(page_path) {
        return page_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
    }

    page_path.with_extension("")
}

pub fn page_listing_parent_path(page_path: &Path) -> PathBuf {
    parent_path(&page_container_path(page_path)).to_path_buf()
}

pub fn named_markdown_page_workspace_entity_path(path: &Path) -> Option<PathBuf> {
    if is_page_document_path(path) {
        return None;
    }
    let extension = path.extension()?.to_str()?;
    if !extension.eq_ignore_ascii_case("md") {
        return None;
    }
    let file_stem = path.file_stem()?.to_str()?;
    let parent = path.parent()?;
    let parent_name = parent.file_name()?.to_str()?;
    if file_stem != parent_name {
        return None;
    }

    let listing_parent = parent_path(parent);
    Some(listing_parent.join(format!("{parent_name}.md")))
}

pub fn parent_path(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| *parent != Path::new(""))
        .unwrap_or_else(|| Path::new(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_markdown_page_workspace_document_maps_back_to_entity_path() {
        assert_eq!(
            named_markdown_page_workspace_entity_path(Path::new(
                "inbox/thread/2026-07-14-hello/2026-07-14-hello.md"
            )),
            Some(PathBuf::from("inbox/thread/2026-07-14-hello.md"))
        );
    }

    #[test]
    fn named_markdown_page_workspace_document_preserves_dotted_page_names() {
        assert_eq!(
            named_markdown_page_workspace_entity_path(Path::new("thread/foo.bar/foo.bar.md")),
            Some(PathBuf::from("thread/foo.bar.md"))
        );
    }

    #[test]
    fn named_markdown_page_workspace_document_ignores_page_md_and_mismatches() {
        assert_eq!(
            named_markdown_page_workspace_entity_path(Path::new("Roadmap/page.md")),
            None
        );
        assert_eq!(
            named_markdown_page_workspace_entity_path(Path::new("thread/foo/bar.md")),
            None
        );
    }
}
