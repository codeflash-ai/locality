//! Portable explicit-root Notion synchronization.
//!
//! The provider search endpoint is intentionally absent from this module. A
//! search result is not an exhaustive Notion inventory, so portable coverage is
//! available only for configured page or full-page database roots.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use locality_connector::{
    NativeEntity, PORTABLE_SCOPE_ROOT_RELATIONSHIP, PortableArtifactKey, PortableBootstrapRequest,
    PortableChangeBatch, PortableCheckpoint, PortableCompleteness, PortableContentArtifact,
    PortableFetchRequest, PortableFetchResult, PortableIncompleteReason,
    PortableProjectionArtifact, PortableRenderRequest, PortableRenderResult, PortableSourceChange,
    PortableSyncRequest,
};
use locality_core::canonical::render_canonical_markdown;
use locality_core::model::{EntityKind, MountId, RemoteId};
use locality_core::portable::{
    LogicalPath, ProjectionFileKind, SourceAction, SourceEdge, SourceObject,
};
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::NotionApi;
use crate::database::{
    database_bundle_provider_version, fetch_database_bundle, render_database_bundle_schema,
};
use crate::dto::{
    BlockDto, BlockTreeDto, FileBlockDto, NotionDatabaseBundle, NotionPageBundle,
    NotionPortableCapturedMediaV1, NotionPortableIncompleteMediaV1, NotionPortablePageBundleV1,
};
use crate::fetch::fetch_known_page_bundle;
use crate::media::{
    PORTABLE_MEDIA_MAX_AGGREGATE_BYTES, PORTABLE_MEDIA_MAX_ASSET_BYTES, PORTABLE_MEDIA_MAX_ASSETS,
    PortableMediaCaptureFetcher, PortableMediaCapturePolicy, default_portable_media_fetcher,
    portable_media_expired, sanitize_portable_hosted_media_url, sanitize_portable_media_type,
};
use crate::projection::enumerate_explicit_root_trees;
use crate::render::{RenderOptions, render_native_entity, render_native_entity_with_options};

const LEGACY_CHECKPOINT_FORMAT_VERSION: u16 = 1;
const CHECKPOINT_FORMAT_VERSION: u16 = 2;
const CHECKPOINT_COMPONENT_VERSION: u16 = 2;
const MAX_EXPLICIT_ROOTS: usize = 16;
const PORTABLE_FORMAT_VERSION: u32 = 1;
const PORTABLE_MEDIA_NATIVE_FORMAT_VERSION: u16 = 1;
const PORTABLE_MEDIA_NATIVE_KIND: &str = "notion_page_portable_media_v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointOperation {
    Bootstrap,
    Synchronize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct LegacyNotionCheckpoint {
    operation: CheckpointOperation,
    root_remote_id: String,
    inventory_sha256: String,
    offset: u64,
    complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct NotionCheckpoint {
    component_version: u16,
    operation: CheckpointOperation,
    root_set_sha256: String,
    root_remote_ids: Vec<String>,
    inventory_sha256: String,
    offset: u64,
    complete: bool,
}

enum DecodedCheckpoint {
    Legacy(LegacyNotionCheckpoint),
    Current(NotionCheckpoint),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CanonicalRootSet {
    roots: Vec<RemoteId>,
    normalized_ids: Vec<String>,
    identity: String,
}

pub(crate) fn bootstrap(
    api: &dyn NotionApi,
    configured_roots: &[RemoteId],
    explicit_root_set: bool,
    request: PortableBootstrapRequest,
) -> LocalityResult<PortableChangeBatch> {
    let roots = validate_explicit_roots(configured_roots, &request.scope.root_remote_ids)?;
    let inventory = inventory(
        api,
        &request.source_connection_id,
        &roots.roots,
        explicit_root_set,
    )?;
    let digest = inventory_sha256(&inventory, explicit_root_set);
    let offset = match request.checkpoint.as_ref() {
        Some(checkpoint) => {
            let checkpoint = decode_checkpoint(checkpoint)?;
            validate_checkpoint(
                &checkpoint,
                CheckpointOperation::Bootstrap,
                &roots,
                Some(&digest),
                explicit_root_set,
            )?;
            usize::try_from(checkpoint_offset(&checkpoint)).map_err(|_| {
                LocalityError::InvalidState(
                    "Notion portable checkpoint offset is too large for this host".to_string(),
                )
            })?
        }
        None => 0,
    };

    page_batch(
        inventory,
        &roots,
        digest,
        CheckpointOperation::Bootstrap,
        offset,
        request.max_changes,
        explicit_root_set,
    )
}

pub(crate) fn synchronize(
    api: &dyn NotionApi,
    configured_roots: &[RemoteId],
    explicit_root_set: bool,
    request: PortableSyncRequest,
) -> LocalityResult<PortableChangeBatch> {
    let roots = validate_explicit_roots(configured_roots, &request.scope.root_remote_ids)?;
    let inventory = inventory(
        api,
        &request.source_connection_id,
        &roots.roots,
        explicit_root_set,
    )?;
    let digest = inventory_sha256(&inventory, explicit_root_set);
    let prior = decode_checkpoint(&request.checkpoint)?;
    validate_checkpoint(
        &prior,
        checkpoint_operation(&prior),
        &roots,
        None,
        explicit_root_set,
    )?;

    let offset = if checkpoint_operation(&prior) == CheckpointOperation::Synchronize
        && !checkpoint_complete(&prior)
    {
        if checkpoint_inventory_sha256(&prior) != digest {
            return Err(LocalityError::InvalidState(
                "Notion portable inventory changed while synchronization was being resumed"
                    .to_string(),
            ));
        }
        usize::try_from(checkpoint_offset(&prior)).map_err(|_| {
            LocalityError::InvalidState(
                "Notion portable checkpoint offset is too large for this host".to_string(),
            )
        })?
    } else {
        // Notion has no exhaustive durable change feed. Revisit the explicit
        // root even when its metadata digest is unchanged so render-time
        // incompleteness (for example media) cannot silently disappear.
        0
    };

    page_batch(
        inventory,
        &roots,
        digest,
        CheckpointOperation::Synchronize,
        offset,
        request.max_changes,
        explicit_root_set,
    )
}

pub(crate) fn fetch(
    api: &dyn NotionApi,
    media_policy: PortableMediaCapturePolicy,
    media_fetcher: Option<&dyn PortableMediaCaptureFetcher>,
    request: PortableFetchRequest,
) -> LocalityResult<PortableFetchResult> {
    match api.retrieve_page(request.remote_id.as_str()) {
        Ok(page) => {
            let bundle = fetch_known_page_bundle(api, request.remote_id.as_str(), page)?;
            if media_policy.captures_hosted_media() {
                fetch_portable_media_page_result(bundle, &request.remote_id, media_fetcher)
            } else {
                fetch_page_result(bundle, &request.remote_id)
            }
        }
        Err(LocalityError::RemoteNotFound(_)) => {
            let bundle = fetch_database_bundle(api, request.remote_id.as_str())?;
            let provider_version = Some(database_bundle_provider_version(&bundle)?);
            let remote_id = RemoteId::new(bundle.database.id.clone());
            let raw = serde_json::to_vec(&bundle).map_err(|error| {
                LocalityError::Io(format!("notion database native encode failed: {error}"))
            })?;

            Ok(PortableFetchResult {
                native: NativeEntity {
                    remote_id,
                    kind: "notion_database".to_string(),
                    raw,
                },
                provider_version,
                completeness: PortableCompleteness::complete(),
            })
        }
        Err(error) => Err(error),
    }
}

fn fetch_portable_media_page_result(
    mut bundle: NotionPageBundle,
    requested_remote_id: &RemoteId,
    media_fetcher: Option<&dyn PortableMediaCaptureFetcher>,
) -> LocalityResult<PortableFetchResult> {
    let provider_version = bundle.page.last_edited_time.clone();
    let remote_id = RemoteId::new(bundle.page.id.clone());
    if !notion_ids_equal(remote_id.as_str(), requested_remote_id.as_str()) {
        return Err(LocalityError::InvalidState(
            "Notion portable fetch returned a different remote object".to_string(),
        ));
    }

    let default_fetcher = default_portable_media_fetcher();
    let fetcher = media_fetcher.unwrap_or(default_fetcher.as_ref());
    let mut capture = PortableMediaCaptureState::new(fetcher);
    for tree in &mut bundle.blocks {
        capture.capture_tree(tree)?;
    }
    capture.sanitize_page_file_properties(&mut bundle)?;

    let completeness = portable_media_completeness(&capture.incomplete);
    let portable_bundle = NotionPortablePageBundleV1 {
        format_version: PORTABLE_MEDIA_NATIVE_FORMAT_VERSION,
        page: bundle,
        captured_media: capture.captured,
        incomplete_media: capture.incomplete,
    };
    let raw = serde_json::to_vec(&portable_bundle)
        .map_err(|_| LocalityError::Io("Notion portable media native encode failed".to_string()))?;

    Ok(PortableFetchResult {
        native: NativeEntity {
            remote_id,
            kind: PORTABLE_MEDIA_NATIVE_KIND.to_string(),
            raw,
        },
        provider_version,
        completeness,
    })
}

struct PortableMediaCaptureState<'a> {
    fetcher: &'a dyn PortableMediaCaptureFetcher,
    captured: Vec<NotionPortableCapturedMediaV1>,
    incomplete: Vec<NotionPortableIncompleteMediaV1>,
    seen_block_ids: BTreeSet<String>,
    asset_count: usize,
    aggregate_bytes: usize,
}

impl<'a> PortableMediaCaptureState<'a> {
    fn new(fetcher: &'a dyn PortableMediaCaptureFetcher) -> Self {
        Self {
            fetcher,
            captured: Vec::new(),
            incomplete: Vec::new(),
            seen_block_ids: BTreeSet::new(),
            asset_count: 0,
            aggregate_bytes: 0,
        }
    }

    fn capture_tree(&mut self, tree: &mut BlockTreeDto) -> LocalityResult<()> {
        let block_id = tree.block.id.clone();
        let kind = tree.block.kind.clone();
        if is_media_kind(&kind) {
            if !self.seen_block_ids.insert(block_id.clone()) {
                return Err(LocalityError::InvalidState(
                    "Notion portable media contains a duplicate block identity".to_string(),
                ));
            }
            self.asset_count += 1;
            if self.asset_count > PORTABLE_MEDIA_MAX_ASSETS {
                return Err(LocalityError::InvalidState(format!(
                    "Notion portable media exceeds the {PORTABLE_MEDIA_MAX_ASSETS}-asset pilot limit"
                )));
            }
            let payload = ensure_media_payload_mut(&mut tree.block).ok_or_else(|| {
                LocalityError::InvalidState(
                    "Notion portable media kind cannot select its payload".to_string(),
                )
            })?;
            self.capture_payload(&block_id, &kind, payload)?;
        }
        for child in &mut tree.children {
            self.capture_tree(child)?;
        }
        Ok(())
    }

    fn capture_payload(
        &mut self,
        block_id: &str,
        kind: &str,
        payload: &mut FileBlockDto,
    ) -> LocalityResult<()> {
        let external_present = payload.external.is_some();
        if let Some(external) = payload.external.as_mut() {
            external.url.clear();
        }
        let Some(hosted) = payload.file.as_mut() else {
            self.record_incomplete(
                block_id,
                kind,
                if external_present {
                    "external_media"
                } else {
                    "missing_file"
                },
            );
            return Ok(());
        };
        let original_url = hosted.url.clone();
        let expiry_time = hosted.expiry_time.take();
        hosted.url.clear();
        if external_present {
            self.record_incomplete(block_id, kind, "ambiguous_file_source");
            return Ok(());
        }
        if original_url.is_empty() {
            self.record_incomplete(block_id, kind, "missing_url");
            return Ok(());
        }
        let sanitized_url = match sanitize_portable_hosted_media_url(&original_url) {
            Ok(url) => url,
            Err(_) => {
                self.record_incomplete(block_id, kind, "url_denied");
                return Ok(());
            }
        };
        if let Some(expiry_time) = expiry_time.as_deref() {
            match portable_media_expired(expiry_time) {
                Ok(false) => {}
                Ok(true) => {
                    self.record_incomplete(block_id, kind, "expired");
                    return Ok(());
                }
                Err(_) => {
                    self.record_incomplete(block_id, kind, "invalid_expiry");
                    return Ok(());
                }
            }
        }
        let captured = match self
            .fetcher
            .fetch(&original_url, PORTABLE_MEDIA_MAX_ASSET_BYTES)
        {
            Ok(captured) => captured,
            Err(_) => {
                self.record_incomplete(block_id, kind, "fetch_failed");
                return Ok(());
            }
        };
        if captured.bytes.len() > PORTABLE_MEDIA_MAX_ASSET_BYTES {
            self.record_incomplete(block_id, kind, "asset_too_large");
            return Ok(());
        }
        let Some(aggregate_bytes) =
            checked_portable_media_aggregate(self.aggregate_bytes, captured.bytes.len())
        else {
            self.record_incomplete(block_id, kind, "aggregate_too_large");
            return Ok(());
        };
        self.aggregate_bytes = aggregate_bytes;
        hosted.url = sanitized_url;
        self.captured.push(NotionPortableCapturedMediaV1 {
            block_id: block_id.to_string(),
            kind: kind.to_string(),
            media_type: sanitize_portable_media_type(Some(&captured.media_type)),
            bytes: captured.bytes,
        });
        Ok(())
    }

    fn sanitize_page_file_properties(
        &mut self,
        bundle: &mut NotionPageBundle,
    ) -> LocalityResult<()> {
        let mut property_asset_index = 0_usize;
        for property in bundle.page.properties.values_mut() {
            for file in &mut property.files {
                property_asset_index += 1;
                self.asset_count += 1;
                if self.asset_count > PORTABLE_MEDIA_MAX_ASSETS {
                    return Err(LocalityError::InvalidState(format!(
                        "Notion portable media exceeds the {PORTABLE_MEDIA_MAX_ASSETS}-asset pilot limit"
                    )));
                }
                if let Some(external) = file.external.as_mut() {
                    external.url.clear();
                }
                if let Some(hosted) = file.file.as_mut() {
                    hosted.url.clear();
                    hosted.expiry_time = None;
                }
                self.record_incomplete(
                    &format!("page-property-file-{property_asset_index}"),
                    "file_property",
                    "unsupported_page_property_media",
                );
            }
        }
        Ok(())
    }

    fn record_incomplete(&mut self, block_id: &str, kind: &str, code: &str) {
        self.incomplete.push(NotionPortableIncompleteMediaV1 {
            block_id: block_id.to_string(),
            kind: kind.to_string(),
            code: code.to_string(),
        });
    }
}

fn portable_media_completeness(
    incomplete: &[NotionPortableIncompleteMediaV1],
) -> PortableCompleteness {
    let mut completeness = PortableCompleteness::complete();
    for media in incomplete {
        completeness.merge(PortableCompleteness::incomplete(
            PortableIncompleteReason::ConnectorLimitation {
                code: format!("notion_media_{}", media.code),
                remote_id: Some(RemoteId::new(media.block_id.clone())),
            },
        ));
    }
    completeness
}

fn ensure_media_payload_mut(block: &mut BlockDto) -> Option<&mut FileBlockDto> {
    match block.kind.as_str() {
        "image" => Some(block.image.get_or_insert_with(FileBlockDto::default)),
        "video" => Some(block.video.get_or_insert_with(FileBlockDto::default)),
        "file" => Some(block.file.get_or_insert_with(FileBlockDto::default)),
        "pdf" => Some(block.pdf.get_or_insert_with(FileBlockDto::default)),
        "audio" => Some(block.audio.get_or_insert_with(FileBlockDto::default)),
        _ => None,
    }
}

fn is_media_kind(kind: &str) -> bool {
    matches!(kind, "image" | "video" | "file" | "pdf" | "audio")
}

fn checked_portable_media_aggregate(current: usize, next: usize) -> Option<usize> {
    current
        .checked_add(next)
        .filter(|total| *total <= PORTABLE_MEDIA_MAX_AGGREGATE_BYTES)
}

fn fetch_page_result(
    bundle: NotionPageBundle,
    requested_remote_id: &RemoteId,
) -> LocalityResult<PortableFetchResult> {
    let provider_version = bundle.page.last_edited_time.clone();
    let remote_id = RemoteId::new(bundle.page.id.clone());
    if !notion_ids_equal(remote_id.as_str(), requested_remote_id.as_str()) {
        return Err(LocalityError::InvalidState(
            "Notion portable fetch returned a different remote object".to_string(),
        ));
    }
    let raw = serde_json::to_vec(&bundle)
        .map_err(|error| LocalityError::Io(format!("notion native encode failed: {error}")))?;

    Ok(PortableFetchResult {
        native: NativeEntity {
            remote_id,
            kind: "notion_page".to_string(),
            raw,
        },
        provider_version,
        completeness: PortableCompleteness::complete(),
    })
}

pub(crate) fn render(request: &PortableRenderRequest) -> LocalityResult<PortableRenderResult> {
    if request.format_version != PORTABLE_FORMAT_VERSION {
        return Err(LocalityError::Unsupported(
            "Notion portable render format version",
        ));
    }
    match request.native.kind.as_str() {
        "notion_page" => render_page(request),
        PORTABLE_MEDIA_NATIVE_KIND => render_portable_media_page(request),
        "notion_database" => render_database(request),
        kind => Err(LocalityError::InvalidState(format!(
            "Notion portable render received unsupported native kind `{kind}`"
        ))),
    }
}

fn render_portable_media_page(
    request: &PortableRenderRequest,
) -> LocalityResult<PortableRenderResult> {
    let bundle = serde_json::from_slice::<NotionPortablePageBundleV1>(&request.native.raw)
        .map_err(|_| LocalityError::Io("Notion portable media native decode failed".to_string()))?;
    if bundle.format_version != PORTABLE_MEDIA_NATIVE_FORMAT_VERSION {
        return Err(LocalityError::InvalidState(
            "Notion portable media native format version is unsupported".to_string(),
        ));
    }
    if !notion_ids_equal(&bundle.page.page.id, request.native.remote_id.as_str()) {
        return Err(LocalityError::InvalidState(
            "Notion portable media page native payload does not match its remote ID".to_string(),
        ));
    }
    validate_portable_media_bundle(&bundle)?;

    let captured_block_ids = bundle
        .captured_media
        .iter()
        .map(|media| media.block_id.clone())
        .collect::<BTreeSet<_>>();
    let page_native = NativeEntity {
        remote_id: request.native.remote_id.clone(),
        kind: "notion_page".to_string(),
        raw: serde_json::to_vec(&bundle.page).map_err(|_| {
            LocalityError::Io("Notion sanitized page native encode failed".to_string())
        })?,
    };
    let rendered = render_native_entity_with_options(
        &page_native,
        &RenderOptions::with_page_path(Path::new(request.logical_path.as_str()))
            .with_local_media_block_ids(captured_block_ids),
    )?;
    let canonical_bytes = render_canonical_markdown(&rendered.document).into_bytes();
    let canonical = PortableContentArtifact {
        artifact_key: artifact_key(
            &request.native.remote_id,
            "canonical_markdown",
            request.format_version,
        ),
        media_type: "text/markdown; charset=utf-8".to_string(),
        body: canonical_bytes.clone(),
    };
    let mut projections = vec![PortableProjectionArtifact {
        artifact: PortableContentArtifact {
            artifact_key: artifact_key(
                &request.native.remote_id,
                "page_markdown",
                request.format_version,
            ),
            media_type: "text/markdown; charset=utf-8".to_string(),
            body: canonical_bytes,
        },
        logical_path: request.logical_path.clone(),
        file_kind: ProjectionFileKind::Markdown,
        format_version: request.format_version,
        supported_actions: [SourceAction::Read, SourceAction::Search]
            .into_iter()
            .collect(),
    }];

    let mut captured_by_block = bundle
        .captured_media
        .into_iter()
        .map(|media| (media.block_id.clone(), media))
        .collect::<BTreeMap<_, _>>();
    let mut projected_paths = BTreeSet::new();
    for rendered_asset in rendered.media_assets {
        let Some(captured) = captured_by_block.remove(&rendered_asset.block_id) else {
            return Err(LocalityError::InvalidState(
                "Notion portable media render produced an uncaptured asset".to_string(),
            ));
        };
        if captured.kind != rendered_asset.kind {
            return Err(LocalityError::InvalidState(
                "Notion portable media kind does not match its block".to_string(),
            ));
        }
        let logical_path = rendered_asset
            .local_path
            .to_str()
            .ok_or_else(|| {
                LocalityError::InvalidState(
                    "Notion portable media path is not valid UTF-8".to_string(),
                )
            })?
            .replace('\\', "/");
        if !projected_paths.insert(logical_path.clone()) {
            return Err(LocalityError::InvalidState(
                "Notion portable media paths collide".to_string(),
            ));
        }
        projections.push(PortableProjectionArtifact {
            artifact: PortableContentArtifact {
                artifact_key: media_artifact_key(
                    &request.native.remote_id,
                    &captured.block_id,
                    request.format_version,
                ),
                media_type: captured.media_type,
                body: captured.bytes,
            },
            logical_path: LogicalPath::new(logical_path).map_err(|_| {
                LocalityError::InvalidState(
                    "Notion portable media path is not a valid logical path".to_string(),
                )
            })?,
            file_kind: ProjectionFileKind::Binary,
            format_version: request.format_version,
            supported_actions: [SourceAction::Read, SourceAction::DownloadAttachment]
                .into_iter()
                .collect(),
        });
    }
    if !captured_by_block.is_empty() {
        return Err(LocalityError::InvalidState(
            "Notion portable media native payload contains an unrendered asset".to_string(),
        ));
    }

    Ok(PortableRenderResult {
        canonical,
        projections,
        completeness: portable_media_completeness(&bundle.incomplete_media),
    })
}

fn validate_portable_media_bundle(bundle: &NotionPortablePageBundleV1) -> LocalityResult<()> {
    let captured = bundle
        .captured_media
        .iter()
        .map(|media| media.block_id.clone())
        .collect::<BTreeSet<_>>();
    if captured.len() != bundle.captured_media.len() {
        return Err(LocalityError::InvalidState(
            "Notion portable media native payload has duplicate captures".to_string(),
        ));
    }
    let incomplete = bundle
        .incomplete_media
        .iter()
        .map(|media| media.block_id.clone())
        .collect::<BTreeSet<_>>();
    if incomplete.len() != bundle.incomplete_media.len() || !captured.is_disjoint(&incomplete) {
        return Err(LocalityError::InvalidState(
            "Notion portable media native payload has ambiguous outcomes".to_string(),
        ));
    }
    if bundle.captured_media.len() + bundle.incomplete_media.len() > PORTABLE_MEDIA_MAX_ASSETS {
        return Err(LocalityError::InvalidState(
            "Notion portable media native payload exceeds the asset limit".to_string(),
        ));
    }
    let aggregate_bytes = bundle
        .captured_media
        .iter()
        .try_fold(0_usize, |total, media| {
            if media.bytes.len() > PORTABLE_MEDIA_MAX_ASSET_BYTES {
                return None;
            }
            total.checked_add(media.bytes.len())
        })
        .filter(|total| *total <= PORTABLE_MEDIA_MAX_AGGREGATE_BYTES)
        .ok_or_else(|| {
            LocalityError::InvalidState(
                "Notion portable media native payload exceeds its byte limits".to_string(),
            )
        })?;
    let _ = aggregate_bytes;
    for media in &bundle.captured_media {
        if sanitize_portable_media_type(Some(&media.media_type)) != media.media_type {
            return Err(LocalityError::InvalidState(
                "Notion portable media type is not canonical".to_string(),
            ));
        }
    }

    let mut media_blocks = BTreeMap::new();
    collect_media_blocks(&bundle.page.blocks, &mut media_blocks)?;
    for (block_id, (kind, payload)) in &media_blocks {
        let is_captured = captured.contains(block_id);
        let is_incomplete = incomplete.contains(block_id);
        if !is_captured && !is_incomplete {
            return Err(LocalityError::InvalidState(
                "Notion portable media block has no capture outcome".to_string(),
            ));
        }
        if payload
            .external
            .as_ref()
            .is_some_and(|file| !file.url.is_empty())
            || payload
                .file
                .as_ref()
                .is_some_and(|file| file.expiry_time.is_some())
        {
            return Err(LocalityError::InvalidState(
                "Notion portable media native payload is not sanitized".to_string(),
            ));
        }
        let hosted_url = payload
            .file
            .as_ref()
            .map(|file| file.url.as_str())
            .unwrap_or("");
        if is_captured {
            let sanitized = sanitize_portable_hosted_media_url(hosted_url).map_err(|_| {
                LocalityError::InvalidState(
                    "Notion portable captured media URL is not allowed".to_string(),
                )
            })?;
            if sanitized != hosted_url {
                return Err(LocalityError::InvalidState(
                    "Notion portable captured media URL is not sanitized".to_string(),
                ));
            }
            let captured_kind = bundle
                .captured_media
                .iter()
                .find(|media| media.block_id == *block_id)
                .map(|media| media.kind.as_str());
            if captured_kind != Some(kind.as_str()) {
                return Err(LocalityError::InvalidState(
                    "Notion portable captured media kind does not match its block".to_string(),
                ));
            }
        } else if !hosted_url.is_empty() {
            return Err(LocalityError::InvalidState(
                "Notion portable incomplete media retained a remote URL".to_string(),
            ));
        }
    }
    for media in &bundle.captured_media {
        if !media_blocks.contains_key(&media.block_id) {
            return Err(LocalityError::InvalidState(
                "Notion portable media capture has no matching block".to_string(),
            ));
        }
    }

    for property in bundle.page.page.properties.values() {
        for file in &property.files {
            if file
                .external
                .as_ref()
                .is_some_and(|file| !file.url.is_empty())
                || file
                    .file
                    .as_ref()
                    .is_some_and(|file| !file.url.is_empty() || file.expiry_time.is_some())
            {
                return Err(LocalityError::InvalidState(
                    "Notion portable page property media is not sanitized".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn collect_media_blocks<'a>(
    trees: &'a [BlockTreeDto],
    media: &mut BTreeMap<String, (String, &'a FileBlockDto)>,
) -> LocalityResult<()> {
    for tree in trees {
        if is_media_kind(&tree.block.kind) {
            let payload = media_payload(&tree.block).ok_or_else(|| {
                LocalityError::InvalidState(
                    "Notion portable media block is missing its payload".to_string(),
                )
            })?;
            if media
                .insert(tree.block.id.clone(), (tree.block.kind.clone(), payload))
                .is_some()
            {
                return Err(LocalityError::InvalidState(
                    "Notion portable media block identity is duplicated".to_string(),
                ));
            }
        }
        collect_media_blocks(&tree.children, media)?;
    }
    Ok(())
}

fn media_payload(block: &BlockDto) -> Option<&FileBlockDto> {
    match block.kind.as_str() {
        "image" => block.image.as_ref(),
        "video" => block.video.as_ref(),
        "file" => block.file.as_ref(),
        "pdf" => block.pdf.as_ref(),
        "audio" => block.audio.as_ref(),
        _ => None,
    }
}

fn render_page(request: &PortableRenderRequest) -> LocalityResult<PortableRenderResult> {
    let native_bundle = serde_json::from_slice::<NotionPageBundle>(&request.native.raw)
        .map_err(|error| LocalityError::Io(format!("notion native decode failed: {error}")))?;
    if !notion_ids_equal(&native_bundle.page.id, request.native.remote_id.as_str()) {
        return Err(LocalityError::InvalidState(
            "Notion portable page native payload does not match its remote ID".to_string(),
        ));
    }
    let contains_media = native_bundle.blocks.iter().any(contains_media_block);
    let rendered = render_native_entity(&request.native)?;
    let canonical_bytes = render_canonical_markdown(&rendered.document).into_bytes();
    let canonical_key = artifact_key(
        &request.native.remote_id,
        "canonical_markdown",
        request.format_version,
    );
    let projection_key = artifact_key(
        &request.native.remote_id,
        "page_markdown",
        request.format_version,
    );
    let canonical = PortableContentArtifact {
        artifact_key: canonical_key,
        media_type: "text/markdown; charset=utf-8".to_string(),
        body: canonical_bytes.clone(),
    };
    let projection = PortableProjectionArtifact {
        artifact: PortableContentArtifact {
            artifact_key: projection_key.clone(),
            media_type: "text/markdown; charset=utf-8".to_string(),
            body: canonical_bytes,
        },
        logical_path: request.logical_path.clone(),
        file_kind: ProjectionFileKind::Markdown,
        format_version: request.format_version,
        supported_actions: [SourceAction::Read, SourceAction::Search]
            .into_iter()
            .collect(),
    };
    let completeness = if contains_media {
        PortableCompleteness::incomplete(PortableIncompleteReason::UnsupportedArtifact {
            artifact_key: projection_key,
            artifact_kind: "notion_media".to_string(),
        })
    } else {
        PortableCompleteness::complete()
    };

    Ok(PortableRenderResult {
        canonical,
        projections: vec![projection],
        completeness,
    })
}

fn render_database(request: &PortableRenderRequest) -> LocalityResult<PortableRenderResult> {
    let bundle =
        serde_json::from_slice::<NotionDatabaseBundle>(&request.native.raw).map_err(|error| {
            LocalityError::Io(format!("notion database native decode failed: {error}"))
        })?;
    validate_database_bundle(&bundle, &request.native.remote_id)?;
    let body = render_database_bundle_schema(&bundle).into_bytes();
    let canonical = PortableContentArtifact {
        artifact_key: database_artifact_key(
            &request.native.remote_id,
            "canonical_schema",
            request.format_version,
        )?,
        media_type: "application/yaml; charset=utf-8".to_string(),
        body: body.clone(),
    };
    let projection = PortableProjectionArtifact {
        artifact: PortableContentArtifact {
            artifact_key: database_artifact_key(
                &request.native.remote_id,
                "database_schema",
                request.format_version,
            )?,
            media_type: "application/yaml; charset=utf-8".to_string(),
            body,
        },
        logical_path: request.logical_path.clone(),
        file_kind: ProjectionFileKind::Yaml,
        format_version: request.format_version,
        supported_actions: [SourceAction::Read, SourceAction::Search]
            .into_iter()
            .collect(),
    };

    Ok(PortableRenderResult {
        canonical,
        projections: vec![projection],
        completeness: PortableCompleteness::complete(),
    })
}

fn validate_database_bundle(
    bundle: &NotionDatabaseBundle,
    remote_id: &RemoteId,
) -> LocalityResult<()> {
    let database_id = canonical_notion_uuid(&bundle.database.id).ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "Notion portable database payload contains non-canonical database ID `{}`",
            bundle.database.id
        ))
    })?;
    let native_remote_id = canonical_notion_uuid(remote_id.as_str()).ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "Notion portable database native entity contains non-canonical remote ID `{}`",
            remote_id.as_str()
        ))
    })?;
    if database_id != native_remote_id {
        return Err(LocalityError::InvalidState(
            "Notion portable database native payload does not match its remote ID".to_string(),
        ));
    }

    let mut declared_data_source_ids = Vec::new();
    for summary in &bundle.database.data_sources {
        let data_source_id = canonical_notion_uuid(&summary.id).ok_or_else(|| {
            LocalityError::InvalidState(format!(
                "Notion portable database payload contains non-canonical data source ID `{}`",
                summary.id
            ))
        })?;
        if !declared_data_source_ids.contains(&data_source_id) {
            declared_data_source_ids.push(data_source_id);
        }
    }

    let mut fetched_data_source_ids = Vec::with_capacity(bundle.data_sources.len());
    for data_source in &bundle.data_sources {
        let data_source_id = canonical_notion_uuid(&data_source.id).ok_or_else(|| {
            LocalityError::InvalidState(format!(
                "Notion portable database payload contains non-canonical data source ID `{}`",
                data_source.id
            ))
        })?;
        if fetched_data_source_ids.contains(&data_source_id) {
            return Err(LocalityError::InvalidState(format!(
                "Notion portable database payload contains duplicate data source `{}`",
                data_source.id
            )));
        }
        let parent_database_id = data_source
            .parent
            .as_ref()
            .and_then(|parent| parent.database_id.as_deref())
            .and_then(canonical_notion_uuid)
            .ok_or_else(|| {
                LocalityError::InvalidState(format!(
                    "Notion portable data source `{}` does not expose a canonical parent database",
                    data_source.id
                ))
            })?;
        if parent_database_id != database_id {
            return Err(LocalityError::InvalidState(format!(
                "Notion portable data source `{}` belongs to a different database",
                data_source.id
            )));
        }
        fetched_data_source_ids.push(data_source_id);
    }

    if declared_data_source_ids != fetched_data_source_ids {
        return Err(LocalityError::InvalidState(
            "Notion portable database payload data sources do not match its summaries".to_string(),
        ));
    }
    Ok(())
}

fn contains_media_block(tree: &BlockTreeDto) -> bool {
    matches!(
        tree.block.kind.as_str(),
        "image" | "video" | "file" | "pdf" | "audio"
    ) || tree.children.iter().any(contains_media_block)
}

pub(crate) fn validate_configured_roots(configured_roots: &[RemoteId]) -> LocalityResult<()> {
    canonical_root_set(configured_roots).map(|_| ())
}

fn validate_explicit_roots(
    configured_roots: &[RemoteId],
    requested_roots: &[RemoteId],
) -> LocalityResult<CanonicalRootSet> {
    if configured_roots.is_empty() {
        return Err(LocalityError::Unsupported(
            "Notion portable bootstrap requires a configured root page or explicit root set",
        ));
    }
    let configured = canonical_root_set(configured_roots)?;
    let requested = canonical_root_set(requested_roots)?;
    if configured.normalized_ids != requested.normalized_ids {
        return Err(LocalityError::InvalidState(
            "Notion portable scope must exactly match the configured explicit root set".to_string(),
        ));
    }
    Ok(configured)
}

fn canonical_root_set(roots: &[RemoteId]) -> LocalityResult<CanonicalRootSet> {
    if roots.is_empty() {
        return Err(LocalityError::InvalidState(
            "Notion explicit root set must not be empty".to_string(),
        ));
    }
    if roots.len() > MAX_EXPLICIT_ROOTS {
        return Err(LocalityError::InvalidState(format!(
            "Notion explicit root set exceeds the limit of {MAX_EXPLICIT_ROOTS}"
        )));
    }
    let mut canonical = BTreeMap::new();
    for root in roots {
        let normalized = normalize_notion_id(root.as_str());
        if normalized.is_empty() {
            return Err(LocalityError::InvalidState(
                "Notion explicit root IDs must not be empty".to_string(),
            ));
        }
        if canonical.insert(normalized.clone(), root.clone()).is_some() {
            return Err(LocalityError::InvalidState(format!(
                "Notion explicit root set contains duplicate root `{}`",
                root.as_str()
            )));
        }
    }
    let normalized_ids = canonical.keys().cloned().collect::<Vec<_>>();
    let mut hasher = Sha256::new();
    for root in &normalized_ids {
        hash_field(&mut hasher, root);
    }
    let identity = format!("sha256:{:x}", hasher.finalize());
    Ok(CanonicalRootSet {
        roots: canonical.into_values().collect(),
        normalized_ids,
        identity,
    })
}

fn inventory(
    api: &dyn NotionApi,
    source_connection_id: &locality_core::portable::SourceConnectionId,
    roots: &[RemoteId],
    include_root_provenance: bool,
) -> LocalityResult<Vec<PortableSourceChange>> {
    // The sentinel mount identity is consumed inside the legacy traversal and
    // is never returned in a portable value.
    let entries = enumerate_explicit_root_trees(api, MountId::new("portable-notion"), roots)?;
    let mut changes = entries
        .into_iter()
        .map(|projected| {
            let entry = projected.entry;
            let logical_path = entry
                .path
                .iter()
                .map(|component| {
                    component.to_str().ok_or_else(|| {
                        LocalityError::InvalidState(
                            "Notion portable projection produced a non-UTF-8 relative path"
                                .to_string(),
                        )
                    })
                })
                .collect::<LocalityResult<Vec<_>>>()?
                .join("/");
            let logical_path = if entry.kind == EntityKind::Database {
                format!("{logical_path}/_schema.yaml")
            } else {
                logical_path
            };
            let logical_path = LogicalPath::new(logical_path).map_err(|error| {
                LocalityError::InvalidState(format!(
                    "Notion portable projection produced an invalid logical path: {error}"
                ))
            })?;
            let requires_fetch = matches!(entry.kind, EntityKind::Page | EntityKind::Database);
            let mut connector_metadata = BTreeMap::new();
            connector_metadata.insert("title".to_string(), entry.title);
            Ok(PortableSourceChange {
                source_object: SourceObject {
                    source_connection_id: source_connection_id.clone(),
                    remote_id: entry.remote_id,
                    kind: entry.kind,
                    edges: include_root_provenance
                        .then(|| SourceEdge {
                            relationship: PORTABLE_SCOPE_ROOT_RELATIONSHIP.to_string(),
                            target_remote_id: RemoteId::new(normalize_notion_id(
                                projected.scope_root_remote_id.as_str(),
                            )),
                        })
                        .into_iter()
                        .collect(),
                    opaque_version: entry.remote_edited_at,
                    deleted: false,
                    connector_metadata,
                    acl_observations: Vec::new(),
                    discovered_at: None,
                    observed_at: None,
                },
                logical_path: Some(logical_path),
                requires_fetch,
            })
        })
        .collect::<LocalityResult<Vec<_>>>()?;
    changes.sort_by(|left, right| {
        left.source_object
            .remote_id
            .cmp(&right.source_object.remote_id)
            .then_with(|| {
                left.logical_path
                    .as_ref()
                    .map(LogicalPath::as_str)
                    .cmp(&right.logical_path.as_ref().map(LogicalPath::as_str))
            })
    });
    Ok(changes)
}

fn page_batch(
    inventory: Vec<PortableSourceChange>,
    roots: &CanonicalRootSet,
    digest: String,
    operation: CheckpointOperation,
    offset: usize,
    max_changes: u32,
    explicit_root_set: bool,
) -> LocalityResult<PortableChangeBatch> {
    if max_changes == 0 {
        return Err(LocalityError::InvalidState(
            "portable connector max_changes must be greater than zero".to_string(),
        ));
    }
    if offset > inventory.len() {
        return Err(LocalityError::InvalidState(
            "Notion portable checkpoint offset exceeds the current inventory".to_string(),
        ));
    }

    let end = offset
        .saturating_add(max_changes as usize)
        .min(inventory.len());
    let mut completeness = PortableCompleteness::complete();
    for change in &inventory {
        if !change.requires_fetch {
            completeness.merge(PortableCompleteness::incomplete(
                PortableIncompleteReason::UnsupportedSourceKind {
                    remote_id: change.source_object.remote_id.clone(),
                    source_kind: source_kind(&change.source_object.kind),
                },
            ));
        }
    }
    if end < inventory.len() {
        completeness.merge(PortableCompleteness::incomplete(
            PortableIncompleteReason::CheckpointContinuation,
        ));
    }

    let checkpoint_offset = u64::try_from(end).map_err(|_| {
        LocalityError::InvalidState(
            "Notion portable inventory is too large to checkpoint".to_string(),
        )
    })?;
    let next_checkpoint = if explicit_root_set {
        encode_checkpoint(&NotionCheckpoint {
            component_version: CHECKPOINT_COMPONENT_VERSION,
            operation,
            root_set_sha256: roots.identity.clone(),
            root_remote_ids: roots.normalized_ids.clone(),
            inventory_sha256: digest,
            offset: checkpoint_offset,
            complete: end == inventory.len(),
        })?
    } else {
        encode_legacy_checkpoint(&LegacyNotionCheckpoint {
            operation,
            root_remote_id: roots.roots[0].as_str().to_string(),
            inventory_sha256: digest,
            offset: checkpoint_offset,
            complete: end == inventory.len(),
        })?
    };

    Ok(PortableChangeBatch {
        changes: inventory[offset..end].to_vec(),
        next_checkpoint,
        completeness,
    })
}

fn source_kind(kind: &EntityKind) -> String {
    match kind {
        EntityKind::Page => "page".to_string(),
        EntityKind::Database => "database".to_string(),
        EntityKind::Directory => "directory".to_string(),
        EntityKind::Asset => "asset".to_string(),
        EntityKind::Unknown(kind) => format!("unknown:{kind}"),
    }
}

fn artifact_key(remote_id: &RemoteId, role: &str, format_version: u32) -> PortableArtifactKey {
    PortableArtifactKey::new(format!(
        "notion:page:{}:{role}:v{format_version}",
        normalize_notion_id(remote_id.as_str())
    ))
}

fn media_artifact_key(
    remote_id: &RemoteId,
    block_id: &str,
    format_version: u32,
) -> PortableArtifactKey {
    PortableArtifactKey::new(format!(
        "notion:page:{}:block:{}:media:v{format_version}",
        normalize_notion_id(remote_id.as_str()),
        normalize_notion_id(block_id)
    ))
}

fn database_artifact_key(
    remote_id: &RemoteId,
    role: &str,
    format_version: u32,
) -> LocalityResult<PortableArtifactKey> {
    let canonical_id = canonical_notion_uuid(remote_id.as_str()).ok_or_else(|| {
        LocalityError::InvalidState(format!(
            "Notion database artifact key requires a canonical remote ID, got `{}`",
            remote_id.as_str()
        ))
    })?;
    Ok(PortableArtifactKey::new(format!(
        "notion:database:{canonical_id}:{role}:v{format_version}"
    )))
}

fn inventory_sha256(inventory: &[PortableSourceChange], include_root_provenance: bool) -> String {
    let mut hasher = Sha256::new();
    for change in inventory {
        hash_field(&mut hasher, change.source_object.remote_id.as_str());
        hash_field(&mut hasher, &source_kind(&change.source_object.kind));
        hash_field(
            &mut hasher,
            change
                .source_object
                .opaque_version
                .as_deref()
                .unwrap_or_default(),
        );
        if include_root_provenance {
            let scope_root = change
                .source_object
                .edges
                .iter()
                .find(|edge| edge.relationship == PORTABLE_SCOPE_ROOT_RELATIONSHIP)
                .expect("explicit-root inventory includes owning-root edge");
            hash_field(&mut hasher, scope_root.target_remote_id.as_str());
        }
        hash_field(
            &mut hasher,
            change
                .logical_path
                .as_ref()
                .map(LogicalPath::as_str)
                .unwrap_or_default(),
        );
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn encode_checkpoint(checkpoint: &NotionCheckpoint) -> LocalityResult<PortableCheckpoint> {
    let opaque = serde_json::to_string(checkpoint).map_err(|error| {
        LocalityError::Io(format!("Notion portable checkpoint encode failed: {error}"))
    })?;
    Ok(PortableCheckpoint {
        format_version: CHECKPOINT_FORMAT_VERSION,
        opaque,
    })
}

fn encode_legacy_checkpoint(
    checkpoint: &LegacyNotionCheckpoint,
) -> LocalityResult<PortableCheckpoint> {
    let opaque = serde_json::to_string(checkpoint).map_err(|error| {
        LocalityError::Io(format!("Notion portable checkpoint encode failed: {error}"))
    })?;
    Ok(PortableCheckpoint {
        format_version: LEGACY_CHECKPOINT_FORMAT_VERSION,
        opaque,
    })
}

fn validate_checkpoint(
    checkpoint: &DecodedCheckpoint,
    operation: CheckpointOperation,
    roots: &CanonicalRootSet,
    inventory_sha256: Option<&str>,
    explicit_root_set: bool,
) -> LocalityResult<()> {
    if let DecodedCheckpoint::Current(checkpoint) = checkpoint
        && checkpoint.component_version > CHECKPOINT_COMPONENT_VERSION
    {
        return Err(LocalityError::InvalidState(format!(
            "Notion portable checkpoint component version {} requires an update",
            checkpoint.component_version
        )));
    }
    let matches = match checkpoint {
        DecodedCheckpoint::Legacy(checkpoint) => {
            !explicit_root_set
                && roots.roots.len() == 1
                && checkpoint.operation == operation
                && notion_ids_equal(&checkpoint.root_remote_id, roots.roots[0].as_str())
                && inventory_sha256.is_none_or(|digest| checkpoint.inventory_sha256 == digest)
        }
        DecodedCheckpoint::Current(checkpoint) => {
            explicit_root_set
                && checkpoint.component_version == CHECKPOINT_COMPONENT_VERSION
                && checkpoint.operation == operation
                && checkpoint.root_remote_ids == roots.normalized_ids
                && checkpoint.root_set_sha256 == roots.identity
                && inventory_sha256.is_none_or(|digest| checkpoint.inventory_sha256 == digest)
        }
    };
    if !matches {
        return Err(LocalityError::InvalidState(
            "Notion portable checkpoint does not match the current root set and inventory"
                .to_string(),
        ));
    }
    Ok(())
}

fn decode_checkpoint(checkpoint: &PortableCheckpoint) -> LocalityResult<DecodedCheckpoint> {
    let decoded = match checkpoint.format_version {
        LEGACY_CHECKPOINT_FORMAT_VERSION => {
            serde_json::from_str(&checkpoint.opaque).map(DecodedCheckpoint::Legacy)
        }
        CHECKPOINT_FORMAT_VERSION => {
            serde_json::from_str(&checkpoint.opaque).map(DecodedCheckpoint::Current)
        }
        version => {
            return Err(LocalityError::InvalidState(format!(
                "Notion portable checkpoint version {version} requires an update (supported: {LEGACY_CHECKPOINT_FORMAT_VERSION}, {CHECKPOINT_FORMAT_VERSION})"
            )));
        }
    };
    decoded.map_err(|_| {
        LocalityError::InvalidState("Notion portable checkpoint is invalid".to_string())
    })
}

fn checkpoint_operation(checkpoint: &DecodedCheckpoint) -> CheckpointOperation {
    match checkpoint {
        DecodedCheckpoint::Legacy(checkpoint) => checkpoint.operation,
        DecodedCheckpoint::Current(checkpoint) => checkpoint.operation,
    }
}

fn checkpoint_inventory_sha256(checkpoint: &DecodedCheckpoint) -> &str {
    match checkpoint {
        DecodedCheckpoint::Legacy(checkpoint) => &checkpoint.inventory_sha256,
        DecodedCheckpoint::Current(checkpoint) => &checkpoint.inventory_sha256,
    }
}

fn checkpoint_offset(checkpoint: &DecodedCheckpoint) -> u64 {
    match checkpoint {
        DecodedCheckpoint::Legacy(checkpoint) => checkpoint.offset,
        DecodedCheckpoint::Current(checkpoint) => checkpoint.offset,
    }
}

fn checkpoint_complete(checkpoint: &DecodedCheckpoint) -> bool {
    match checkpoint {
        DecodedCheckpoint::Legacy(checkpoint) => checkpoint.complete,
        DecodedCheckpoint::Current(checkpoint) => checkpoint.complete,
    }
}

fn normalize_notion_id(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

fn canonical_notion_uuid(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let valid = match bytes.len() {
        32 => bytes.iter().all(u8::is_ascii_hexdigit),
        36 => bytes.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        }),
        _ => false,
    };
    valid.then(|| normalize_notion_id(value))
}

fn notion_ids_equal(left: &str, right: &str) -> bool {
    normalize_notion_id(left) == normalize_notion_id(right)
}

#[cfg(test)]
mod tests {
    use super::{
        FileBlockDto, PORTABLE_MEDIA_MAX_AGGREGATE_BYTES, PortableMediaCaptureFetcher,
        PortableMediaCaptureState, checked_portable_media_aggregate,
    };
    use crate::dto::HostedFileDto;
    use crate::media::PortableMediaCapture;

    #[test]
    fn portable_media_aggregate_limit_is_exact_and_overflow_safe() {
        assert_eq!(
            checked_portable_media_aggregate(PORTABLE_MEDIA_MAX_AGGREGATE_BYTES - 1, 1),
            Some(PORTABLE_MEDIA_MAX_AGGREGATE_BYTES)
        );
        assert_eq!(
            checked_portable_media_aggregate(PORTABLE_MEDIA_MAX_AGGREGATE_BYTES - 1, 2),
            None
        );
        assert_eq!(checked_portable_media_aggregate(usize::MAX, 1), None);
    }

    #[test]
    fn portable_media_aggregate_overflow_clears_url_and_records_incompleteness() {
        struct OneByteFetcher;

        impl PortableMediaCaptureFetcher for OneByteFetcher {
            fn fetch(
                &self,
                _hosted_url: &str,
                _max_bytes: usize,
            ) -> locality_core::LocalityResult<PortableMediaCapture> {
                Ok(PortableMediaCapture {
                    bytes: vec![1],
                    media_type: "image/png".to_string(),
                })
            }
        }

        let fetcher = OneByteFetcher;
        let mut state = PortableMediaCaptureState::new(&fetcher);
        state.aggregate_bytes = PORTABLE_MEDIA_MAX_AGGREGATE_BYTES;
        let mut payload = FileBlockDto {
            kind: "file".to_string(),
            external: None,
            file: Some(HostedFileDto {
                url: "https://secure.notion-static.com/image.png?X-Amz-Signature=secret"
                    .to_string(),
                expiry_time: None,
            }),
            caption: Vec::new(),
        };

        state
            .capture_payload("block-1", "image", &mut payload)
            .expect("aggregate capture");

        assert!(state.captured.is_empty());
        assert_eq!(state.incomplete[0].code, "aggregate_too_large");
        assert_eq!(payload.file.expect("hosted").url, "");
    }
}
