//! Portable explicit-root Notion synchronization.
//!
//! The provider search endpoint is intentionally absent from this module. A
//! search result is not an exhaustive Notion inventory, so portable coverage is
//! available only for a configured root page.

use std::collections::BTreeMap;

use locality_connector::{
    NativeEntity, PortableArtifactKey, PortableBootstrapRequest, PortableChangeBatch,
    PortableCheckpoint, PortableCompleteness, PortableContentArtifact, PortableFetchRequest,
    PortableFetchResult, PortableIncompleteReason, PortableProjectionArtifact,
    PortableRenderRequest, PortableRenderResult, PortableSourceChange, PortableSyncRequest,
};
use locality_core::canonical::render_canonical_markdown;
use locality_core::model::{EntityKind, MountId, RemoteId};
use locality_core::portable::{LogicalPath, ProjectionFileKind, SourceAction, SourceObject};
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::client::NotionApi;
use crate::dto::{BlockTreeDto, NotionPageBundle};
use crate::fetch::fetch_page_bundle;
use crate::projection::enumerate_root_page_tree;
use crate::render::render_native_entity;

const CHECKPOINT_FORMAT_VERSION: u16 = 1;
const PORTABLE_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointOperation {
    Bootstrap,
    Synchronize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct NotionCheckpoint {
    operation: CheckpointOperation,
    root_remote_id: String,
    inventory_sha256: String,
    offset: u64,
    complete: bool,
}

pub(crate) fn bootstrap(
    api: &dyn NotionApi,
    configured_root: Option<&RemoteId>,
    request: PortableBootstrapRequest,
) -> LocalityResult<PortableChangeBatch> {
    let root = validate_explicit_root(configured_root, &request.scope.root_remote_ids)?;
    let inventory = inventory(api, &request.source_connection_id, root)?;
    let digest = inventory_sha256(&inventory);
    let offset = match request.checkpoint.as_ref() {
        Some(checkpoint) => {
            let checkpoint = decode_checkpoint(checkpoint)?;
            validate_checkpoint(&checkpoint, CheckpointOperation::Bootstrap, root, &digest)?;
            usize::try_from(checkpoint.offset).map_err(|_| {
                LocalityError::InvalidState(
                    "Notion portable checkpoint offset is too large for this host".to_string(),
                )
            })?
        }
        None => 0,
    };

    page_batch(
        inventory,
        root,
        digest,
        CheckpointOperation::Bootstrap,
        offset,
        request.max_changes,
    )
}

pub(crate) fn synchronize(
    api: &dyn NotionApi,
    configured_root: Option<&RemoteId>,
    request: PortableSyncRequest,
) -> LocalityResult<PortableChangeBatch> {
    let root = validate_explicit_root(configured_root, &request.scope.root_remote_ids)?;
    let inventory = inventory(api, &request.source_connection_id, root)?;
    let digest = inventory_sha256(&inventory);
    let prior = decode_checkpoint(&request.checkpoint)?;
    if !notion_ids_equal(&prior.root_remote_id, root.as_str()) {
        return Err(LocalityError::InvalidState(
            "Notion portable checkpoint belongs to a different explicit root".to_string(),
        ));
    }

    let offset = if prior.operation == CheckpointOperation::Synchronize && !prior.complete {
        if prior.inventory_sha256 != digest {
            return Err(LocalityError::InvalidState(
                "Notion portable inventory changed while synchronization was being resumed"
                    .to_string(),
            ));
        }
        usize::try_from(prior.offset).map_err(|_| {
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
        root,
        digest,
        CheckpointOperation::Synchronize,
        offset,
        request.max_changes,
    )
}

pub(crate) fn fetch(
    api: &dyn NotionApi,
    request: PortableFetchRequest,
) -> LocalityResult<PortableFetchResult> {
    let bundle = fetch_page_bundle(api, request.remote_id.as_str())?;
    let provider_version = bundle.page.last_edited_time.clone();
    let remote_id = RemoteId::new(bundle.page.id.clone());
    if !notion_ids_equal(remote_id.as_str(), request.remote_id.as_str()) {
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
    let native_bundle = serde_json::from_slice::<NotionPageBundle>(&request.native.raw)
        .map_err(|error| LocalityError::Io(format!("notion native decode failed: {error}")))?;
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

fn contains_media_block(tree: &BlockTreeDto) -> bool {
    matches!(
        tree.block.kind.as_str(),
        "image" | "video" | "file" | "pdf" | "audio"
    ) || tree.children.iter().any(contains_media_block)
}

fn validate_explicit_root<'a>(
    configured_root: Option<&'a RemoteId>,
    requested_roots: &[RemoteId],
) -> LocalityResult<&'a RemoteId> {
    let configured_root = configured_root.ok_or(LocalityError::Unsupported(
        "Notion portable bootstrap requires a configured root page",
    ))?;
    if requested_roots.len() != 1
        || !notion_ids_equal(requested_roots[0].as_str(), configured_root.as_str())
    {
        return Err(LocalityError::InvalidState(
            "Notion portable scope must contain only the configured root page".to_string(),
        ));
    }
    Ok(configured_root)
}

fn inventory(
    api: &dyn NotionApi,
    source_connection_id: &locality_core::portable::SourceConnectionId,
    root: &RemoteId,
) -> LocalityResult<Vec<PortableSourceChange>> {
    // The sentinel mount identity is consumed inside the legacy traversal and
    // is never returned in a portable value.
    let entries = enumerate_root_page_tree(api, MountId::new("portable-notion"), root)?;
    let mut changes = entries
        .into_iter()
        .map(|entry| {
            let logical_path = entry.path.to_str().ok_or_else(|| {
                LocalityError::InvalidState(
                    "Notion portable projection produced a non-UTF-8 relative path".to_string(),
                )
            })?;
            let logical_path = LogicalPath::new(logical_path.to_string()).map_err(|error| {
                LocalityError::InvalidState(format!(
                    "Notion portable projection produced an invalid logical path: {error}"
                ))
            })?;
            let requires_fetch = entry.kind == EntityKind::Page;
            let mut connector_metadata = BTreeMap::new();
            connector_metadata.insert("title".to_string(), entry.title);
            Ok(PortableSourceChange {
                source_object: SourceObject {
                    source_connection_id: source_connection_id.clone(),
                    remote_id: entry.remote_id,
                    kind: entry.kind,
                    edges: Vec::new(),
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
    root: &RemoteId,
    digest: String,
    operation: CheckpointOperation,
    offset: usize,
    max_changes: u32,
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

    let next_checkpoint = encode_checkpoint(&NotionCheckpoint {
        operation,
        root_remote_id: root.as_str().to_string(),
        inventory_sha256: digest,
        offset: u64::try_from(end).map_err(|_| {
            LocalityError::InvalidState(
                "Notion portable inventory is too large to checkpoint".to_string(),
            )
        })?,
        complete: end == inventory.len(),
    })?;

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

fn inventory_sha256(inventory: &[PortableSourceChange]) -> String {
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

fn decode_checkpoint(checkpoint: &PortableCheckpoint) -> LocalityResult<NotionCheckpoint> {
    if checkpoint.format_version != CHECKPOINT_FORMAT_VERSION {
        return Err(LocalityError::InvalidState(format!(
            "Notion portable checkpoint version {} requires an update (supported: {CHECKPOINT_FORMAT_VERSION})",
            checkpoint.format_version
        )));
    }
    serde_json::from_str(&checkpoint.opaque).map_err(|_| {
        LocalityError::InvalidState("Notion portable checkpoint is invalid".to_string())
    })
}

fn validate_checkpoint(
    checkpoint: &NotionCheckpoint,
    operation: CheckpointOperation,
    root: &RemoteId,
    inventory_sha256: &str,
) -> LocalityResult<()> {
    if checkpoint.operation != operation
        || !notion_ids_equal(&checkpoint.root_remote_id, root.as_str())
        || checkpoint.inventory_sha256 != inventory_sha256
    {
        return Err(LocalityError::InvalidState(
            "Notion portable checkpoint does not match the current inventory".to_string(),
        ));
    }
    Ok(())
}

fn normalize_notion_id(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

fn notion_ids_equal(left: &str, right: &str) -> bool {
    normalize_notion_id(left) == normalize_notion_id(right)
}
