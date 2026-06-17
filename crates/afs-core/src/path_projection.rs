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

pub fn parent_path(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| *parent != Path::new(""))
        .unwrap_or_else(|| Path::new(""))
}
