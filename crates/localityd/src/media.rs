use std::path::Path;

use locality_core::LocalityResult;
use locality_core::canonical::render_canonical_markdown;
use locality_core::model::CanonicalDocument;
use locality_notion::media::{
    DownloadedMediaAsset, resolve_media_href_with_content_root, update_media_manifest,
};

use crate::hydration::HydratedAsset;

pub fn update_hydrated_media_manifest(root: &Path, assets: &[HydratedAsset]) -> LocalityResult<()> {
    let downloaded = assets
        .iter()
        .filter_map(|asset| {
            let media = asset.media.as_ref()?;
            Some(DownloadedMediaAsset {
                block_id: media.block_id.clone(),
                kind: media.kind.clone(),
                source_url: media.source_url.clone(),
                local_path: asset.path.clone(),
                bytes: asset.bytes.clone(),
            })
        })
        .collect::<Vec<_>>();
    update_media_manifest(root, &downloaded)
}

pub fn render_document_with_absolute_media_hrefs(
    document: &CanonicalDocument,
    page_path: &Path,
    output_root: &Path,
) -> String {
    render_canonical_markdown(&document_with_absolute_media_hrefs(
        document,
        page_path,
        output_root,
    ))
}

pub fn document_with_absolute_media_hrefs(
    document: &CanonicalDocument,
    page_path: &Path,
    output_root: &Path,
) -> CanonicalDocument {
    let mut document = document.clone();
    document.body = absolutize_media_hrefs(&document.body, page_path, output_root);
    document
}

pub(crate) fn has_missing_local_media_hrefs(
    markdown: &str,
    page_path: &Path,
    output_root: &Path,
) -> bool {
    local_media_hrefs(markdown).into_iter().any(|href| {
        resolve_media_href_with_content_root(page_path, href, output_root)
            .is_some_and(|local_path| !output_root.join(local_path).exists())
    })
}

fn absolutize_media_hrefs(body: &str, page_path: &Path, output_root: &Path) -> String {
    let mut rewritten = String::with_capacity(body.len());
    let mut rest = body;

    while let Some(link_start) = rest.find("](") {
        rewritten.push_str(&rest[..link_start + 2]);
        let target_start = link_start + 2;
        let target_and_tail = &rest[target_start..];
        let Some(target_end) = target_and_tail.find(')') else {
            rewritten.push_str(target_and_tail);
            return rewritten;
        };

        let target = &target_and_tail[..target_end];
        if let Some(local_path) =
            resolve_media_href_with_content_root(page_path, target, output_root)
        {
            rewritten.push_str(&absolute_media_href(output_root, &local_path));
        } else {
            rewritten.push_str(target);
        }
        rewritten.push(')');
        rest = &target_and_tail[target_end + 1..];
    }

    rewritten.push_str(rest);
    rewritten
}

fn local_media_hrefs(markdown: &str) -> Vec<&str> {
    let mut hrefs = Vec::new();
    let mut offset = 0;

    while let Some(label_end_offset) = markdown[offset..].find("](") {
        let label_end = offset + label_end_offset;
        let href_start = label_end + 2;
        let Some(href_end) = find_markdown_link_href_end(markdown, href_start) else {
            break;
        };
        hrefs.push(&markdown[href_start..href_end]);
        offset = href_end + 1;
    }

    hrefs
}

fn find_markdown_link_href_end(input: &str, href_start: usize) -> Option<usize> {
    let mut escaped = false;
    let mut paren_depth = 0usize;

    for (offset, ch) in input[href_start..].char_indices() {
        let index = href_start + offset;
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match ch {
            '(' => paren_depth += 1,
            ')' if paren_depth == 0 => return Some(index),
            ')' => paren_depth -= 1,
            _ => {}
        }
    }

    None
}

fn absolute_media_href(output_root: &Path, local_path: &Path) -> String {
    output_root
        .join(local_path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::{document_with_absolute_media_hrefs, has_missing_local_media_hrefs};
    use locality_core::model::CanonicalDocument;
    use std::path::Path;

    #[test]
    fn absolute_media_hrefs_use_forward_slashes_for_windows_style_output_roots() {
        let document = CanonicalDocument::new(
            "",
            "![Image](../.loc/media/Roadmap/image-1.png)\n".to_string(),
        );
        let rewritten = document_with_absolute_media_hrefs(
            &document,
            Path::new("Roadmap/page.md"),
            Path::new(r"C:\Users\runner\AppData\Local\Temp\loc\.content\notion-main\files"),
        );

        assert_eq!(
            rewritten.body,
            "![Image](C:/Users/runner/AppData/Local/Temp/loc/.content/notion-main/files/.loc/media/Roadmap/image-1.png)\n"
        );
    }

    #[test]
    fn detects_missing_local_media_hrefs_with_escaped_parentheses() {
        let root = std::env::temp_dir().join(format!("loc-media-missing-{}", std::process::id()));
        let media_path = root.join(".loc/media/Roadmap/image (1).png");
        std::fs::create_dir_all(media_path.parent().expect("media parent")).expect("mkdir media");
        std::fs::write(&media_path, b"image").expect("write media");

        assert!(!has_missing_local_media_hrefs(
            "![Image](.loc/media/Roadmap/image \\(1\\).png)\n",
            Path::new("page.md"),
            &root,
        ));
        std::fs::remove_file(&media_path).expect("remove media");
        assert!(has_missing_local_media_hrefs(
            "![Image](.loc/media/Roadmap/image \\(1\\).png)\n",
            Path::new("page.md"),
            &root,
        ));

        let _ = std::fs::remove_dir_all(root);
    }
}
