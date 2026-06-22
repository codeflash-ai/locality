use std::path::Path;

use afs_core::AfsResult;
use afs_core::canonical::render_canonical_markdown;
use afs_core::model::CanonicalDocument;
use afs_notion::media::{
    DownloadedMediaAsset, resolve_media_href_with_content_root, update_media_manifest,
};

use crate::hydration::HydratedAsset;

pub fn update_hydrated_media_manifest(root: &Path, assets: &[HydratedAsset]) -> AfsResult<()> {
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
            rewritten.push_str(&output_root.join(local_path).display().to_string());
        } else {
            rewritten.push_str(target);
        }
        rewritten.push(')');
        rest = &target_and_tail[target_end + 1..];
    }

    rewritten.push_str(rest);
    rewritten
}
