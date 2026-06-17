use std::path::Path;

use afs_core::AfsResult;
use afs_notion::media::{DownloadedMediaAsset, update_media_manifest};

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
