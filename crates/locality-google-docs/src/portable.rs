use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use locality_connector::{
    NativeEntity, PORTABLE_SCOPE_ROOT_RELATIONSHIP, PortableArtifactKey, PortableBootstrapRequest,
    PortableChangeBatch, PortableCheckpoint, PortableCompleteness, PortableContentArtifact,
    PortableFetchRequest, PortableFetchResult, PortableProjectionArtifact, PortableRenderRequest,
    PortableRenderResult, PortableSourceChange,
};
use locality_core::canonical::render_canonical_markdown;
use locality_core::model::{EntityKind, RemoteId, TreeEntry};
use locality_core::portable::{
    LogicalPath, ProjectionFileKind, SourceAction, SourceConnectionId, SourceEdge, SourceObject,
};
use locality_core::{LocalityError, LocalityResult};

use crate::client::GoogleDriveApi;
use crate::connector::{
    GoogleDocsConnector, incomplete_drive_search_error, project_drive_children,
};
use crate::drive_dto::DriveFile;
use crate::render::{GoogleDocsNativeBundle, combined_remote_version, render_google_document};

const GOOGLE_DOCS_PORTABLE_CHECKPOINT_VERSION: u16 = 1;
const GOOGLE_DOCS_PORTABLE_NATIVE_KIND: &str = "google_docs_portable_document";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct GoogleDocsPortableNativeBundle {
    root_remote_id: RemoteId,
    bundle: GoogleDocsNativeBundle,
}

pub(crate) fn bootstrap_google_docs_portable(
    connector: &GoogleDocsConnector,
    request: PortableBootstrapRequest,
) -> LocalityResult<PortableChangeBatch> {
    let configured_root_id = portable_workspace_root_id(connector)?;
    if request.scope.root_remote_ids.as_slice() != [configured_root_id.clone()] {
        return Err(LocalityError::Unsupported(
            "Google Docs portable bootstrap requires the configured Drive-folder root",
        ));
    }
    if request.checkpoint.is_some() {
        return Err(LocalityError::Unsupported(
            "Google Docs portable bootstrap does not accept checkpoints",
        ));
    }
    if request.max_changes == 0 {
        return Err(LocalityError::InvalidState(
            "Google Docs portable bootstrap max_changes must be greater than 0".to_string(),
        ));
    }

    let entries = enumerate_portable_drive_tree(
        connector.drive_api(),
        &locality_core::model::MountId::new("google-docs-portable"),
        configured_root_id.as_str(),
        Path::new(""),
    )?;
    let mut changes =
        portable_google_docs_changes(&request.source_connection_id, &configured_root_id, entries)?;
    changes.sort_by(|left, right| {
        left.logical_path
            .as_ref()
            .map(LogicalPath::as_str)
            .cmp(&right.logical_path.as_ref().map(LogicalPath::as_str))
            .then_with(|| {
                left.source_object
                    .remote_id
                    .cmp(&right.source_object.remote_id)
            })
    });
    if changes.len() > request.max_changes as usize {
        return Err(LocalityError::Unsupported(
            "Google Docs portable bootstrap cannot publish a truncated batch without continuation; increase max_changes",
        ));
    }

    Ok(PortableChangeBatch {
        changes,
        next_checkpoint: google_docs_portable_checkpoint(&configured_root_id),
        completeness: PortableCompleteness::complete(),
    })
}

pub(crate) fn fetch_google_docs_portable(
    connector: &GoogleDocsConnector,
    request: PortableFetchRequest,
) -> LocalityResult<PortableFetchResult> {
    let root_id = portable_workspace_root_id(connector)?;
    let drive_file = connector.drive_api().get_file(request.remote_id.as_str())?;
    if drive_file.id != request.remote_id.as_str() {
        return Err(LocalityError::InvalidState(format!(
            "Google Docs portable fetch returned Drive id `{}` for requested `{}`",
            drive_file.id,
            request.remote_id.as_str()
        )));
    }
    if drive_file.trashed || !drive_file.is_google_doc() {
        return Err(LocalityError::Unsupported(
            "Google Docs portable fetch requires an active Google Docs file",
        ));
    }
    ensure_drive_file_within_root(connector.drive_api(), &drive_file, &root_id)?;

    let document = connector
        .docs_api()
        .get_document(request.remote_id.as_str())?;
    if document.document_id != request.remote_id.as_str() {
        return Err(LocalityError::InvalidState(format!(
            "Google Docs portable fetch returned document id `{}` for requested `{}`",
            document.document_id,
            request.remote_id.as_str()
        )));
    }
    let provider_version = combined_remote_version(&drive_file, document.revision_id.as_deref());
    if provider_version == "unknown" {
        return Err(LocalityError::InvalidState(format!(
            "Google Docs portable fetch requires a stable provider version for `{}`",
            request.remote_id.as_str()
        )));
    }
    let raw = serde_json::to_vec(&GoogleDocsPortableNativeBundle {
        root_remote_id: root_id,
        bundle: GoogleDocsNativeBundle {
            drive_file,
            document,
        },
    })
    .map_err(|error| {
        LocalityError::Io(format!(
            "google docs portable native encode failed: {error}"
        ))
    })?;

    Ok(PortableFetchResult {
        native: NativeEntity {
            remote_id: request.remote_id,
            kind: GOOGLE_DOCS_PORTABLE_NATIVE_KIND.to_string(),
            raw,
        },
        provider_version: Some(provider_version),
        completeness: PortableCompleteness::complete(),
    })
}

pub(crate) fn render_google_docs_portable(
    connector: &GoogleDocsConnector,
    request: &PortableRenderRequest,
) -> LocalityResult<PortableRenderResult> {
    if request.native.kind != GOOGLE_DOCS_PORTABLE_NATIVE_KIND {
        return Err(LocalityError::Unsupported(
            "Google Docs portable render requires a portable Google Docs native bundle",
        ));
    }
    let portable = serde_json::from_slice::<GoogleDocsPortableNativeBundle>(&request.native.raw)
        .map_err(|error| {
            LocalityError::Io(format!(
                "google docs portable native decode failed: {error}"
            ))
        })?;
    let configured_root_id = portable_workspace_root_id(connector)?;
    if portable.root_remote_id != configured_root_id {
        return Err(LocalityError::InvalidState(format!(
            "Google Docs portable native root `{}` did not match configured root `{}`",
            portable.root_remote_id.as_str(),
            configured_root_id.as_str()
        )));
    }
    if portable.bundle.drive_file.id != request.native.remote_id.as_str()
        || portable.bundle.document.document_id != request.native.remote_id.as_str()
    {
        return Err(LocalityError::InvalidState(format!(
            "Google Docs portable native remote id `{}` did not match bundled document",
            request.native.remote_id.as_str()
        )));
    }
    if portable.bundle.drive_file.trashed || !portable.bundle.drive_file.is_google_doc() {
        return Err(LocalityError::Unsupported(
            "Google Docs portable render requires an active Google Docs file",
        ));
    }

    let document = render_google_document(&portable.bundle)?.document;
    let canonical = PortableContentArtifact {
        artifact_key: google_docs_artifact_key(
            &request.native.remote_id,
            "canonical",
            request.format_version,
        ),
        media_type: "application/json".to_string(),
        body: request.native.raw.clone(),
    };
    let projections = vec![PortableProjectionArtifact {
        artifact: PortableContentArtifact {
            artifact_key: google_docs_artifact_key(
                &request.native.remote_id,
                "markdown",
                request.format_version,
            ),
            media_type: "text/markdown; charset=utf-8".to_string(),
            body: render_canonical_markdown(&document).into_bytes(),
        },
        logical_path: request.logical_path.clone(),
        file_kind: ProjectionFileKind::Markdown,
        format_version: request.format_version,
        supported_actions: [SourceAction::Read, SourceAction::Search]
            .into_iter()
            .collect(),
    }];

    Ok(PortableRenderResult {
        canonical,
        projections,
        completeness: PortableCompleteness::complete(),
    })
}

fn portable_workspace_root_id(connector: &GoogleDocsConnector) -> LocalityResult<RemoteId> {
    let root_id = connector
        .portable_workspace_folder_id()
        .cloned()
        .ok_or_else(|| {
            LocalityError::InvalidState(
                "google docs portable export is missing workspace folder id".to_string(),
            )
        })?;
    let root = connector.drive_api().get_file(root_id.as_str())?;
    if root.id != root_id.as_str() {
        return Err(LocalityError::InvalidState(format!(
            "Google Docs portable root `{}` resolved to different Drive id `{}`",
            root_id.as_str(),
            root.id
        )));
    }
    if root.trashed {
        return Err(LocalityError::RemoteNotFound(format!(
            "Google Docs portable root `{}` is trashed",
            root_id.as_str()
        )));
    }
    if !root.is_folder() {
        return Err(LocalityError::Guardrail(format!(
            "Google Docs portable root `{}` is not a Google Drive folder",
            root_id.as_str()
        )));
    }
    if root.remote_version().is_none() {
        return Err(LocalityError::InvalidState(format!(
            "Google Docs portable root `{}` does not have a stable provider version",
            root_id.as_str()
        )));
    }
    Ok(root_id)
}

fn enumerate_portable_drive_tree(
    drive: &dyn GoogleDriveApi,
    mount_id: &locality_core::model::MountId,
    parent_id: &str,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let mut entries = BTreeMap::new();
    enumerate_portable_drive_tree_into(drive, mount_id, parent_id, parent_path, &mut entries)?;
    Ok(entries.into_values().collect())
}

fn enumerate_portable_drive_tree_into(
    drive: &dyn GoogleDriveApi,
    mount_id: &locality_core::model::MountId,
    parent_id: &str,
    parent_path: &Path,
    entries: &mut BTreeMap<RemoteId, TreeEntry>,
) -> LocalityResult<()> {
    for entry in list_portable_drive_children(drive, mount_id, parent_id, parent_path)? {
        let is_directory = entry.kind == EntityKind::Directory;
        let remote_id = entry.remote_id.clone();
        let directory_path = entry.path.clone();
        if let Some(existing) = entries.get(&remote_id) {
            ensure_portable_tree_entry_facts_agree(existing, &entry)?;
            continue;
        }
        entries.insert(remote_id.clone(), entry);
        if is_directory {
            enumerate_portable_drive_tree_into(
                drive,
                mount_id,
                remote_id.as_str(),
                &directory_path,
                entries,
            )?;
        }
    }
    Ok(())
}

fn list_portable_drive_children(
    drive: &dyn GoogleDriveApi,
    mount_id: &locality_core::model::MountId,
    parent_id: &str,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let mut cursor = None;
    let mut seen_page_tokens = BTreeSet::new();
    let mut files = BTreeMap::new();
    loop {
        let page = drive.list_children(parent_id, cursor.as_deref())?;
        if page.incomplete_search {
            return Err(incomplete_drive_search_error());
        }
        for file in page.files.into_iter().filter(|file| !file.trashed) {
            if !file.is_folder() && !file.is_google_doc() {
                return Err(LocalityError::Guardrail(format!(
                    "Google Docs portable bootstrap does not support scoped Drive child `{}` of type `{}`",
                    file.id, file.mime_type
                )));
            }
            if let Some(existing) = files.get(&file.id) {
                ensure_portable_drive_file_facts_agree(existing, &file)?;
            } else {
                files.insert(file.id.clone(), file);
            }
        }
        let Some(next_page_token) = page.next_page_token else {
            break;
        };
        if !seen_page_tokens.insert(next_page_token.clone()) {
            return Err(LocalityError::Io(format!(
                "Google Docs portable bootstrap pagination returned repeated page token `{next_page_token}` for parent `{parent_id}`"
            )));
        }
        cursor = Some(next_page_token);
    }
    let mut files = files.into_values().collect::<Vec<_>>();
    files.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(project_drive_children(mount_id, parent_path, files))
}

fn ensure_portable_drive_file_facts_agree(
    existing: &DriveFile,
    candidate: &DriveFile,
) -> LocalityResult<()> {
    let conflicting_fact = if existing.mime_type != candidate.mime_type {
        Some("identity")
    } else if existing.remote_version() != candidate.remote_version() {
        Some("version")
    } else if existing.name != candidate.name || existing.parents != candidate.parents {
        Some("path")
    } else {
        None
    };
    match conflicting_fact {
        Some(fact) => Err(LocalityError::InvalidState(format!(
            "Google Docs portable bootstrap found conflicting duplicate inventory entry `{}`: {fact} disagrees",
            existing.id
        ))),
        None => Ok(()),
    }
}

fn ensure_portable_tree_entry_facts_agree(
    existing: &TreeEntry,
    candidate: &TreeEntry,
) -> LocalityResult<()> {
    let conflicting_fact = if existing.kind != candidate.kind {
        Some("identity")
    } else if existing.remote_edited_at != candidate.remote_edited_at {
        Some("version")
    } else if existing.path != candidate.path {
        Some("path")
    } else {
        None
    };
    match conflicting_fact {
        Some(fact) => Err(LocalityError::InvalidState(format!(
            "Google Docs portable bootstrap found conflicting duplicate inventory entry `{}`: {fact} disagrees",
            existing.remote_id.as_str()
        ))),
        None => Ok(()),
    }
}

fn portable_google_docs_changes(
    source_connection_id: &SourceConnectionId,
    root_id: &RemoteId,
    entries: Vec<TreeEntry>,
) -> LocalityResult<Vec<PortableSourceChange>> {
    entries
        .into_iter()
        .filter(|entry| entry.kind == EntityKind::Page)
        .map(|entry| {
            let logical_path = LogicalPath::new(entry.path.to_string_lossy().replace('\\', "/"))
                .map_err(|error| {
                    LocalityError::InvalidState(format!(
                        "Google Docs portable logical path is invalid: {error}"
                    ))
                })?;
            Ok(PortableSourceChange {
                source_object: SourceObject {
                    source_connection_id: source_connection_id.clone(),
                    remote_id: entry.remote_id,
                    kind: EntityKind::Page,
                    edges: vec![SourceEdge {
                        relationship: PORTABLE_SCOPE_ROOT_RELATIONSHIP.to_string(),
                        target_remote_id: root_id.clone(),
                    }],
                    opaque_version: entry.remote_edited_at,
                    deleted: false,
                    connector_metadata: BTreeMap::new(),
                    acl_observations: Vec::new(),
                    discovered_at: None,
                    observed_at: None,
                },
                logical_path: Some(logical_path),
                requires_fetch: true,
            })
        })
        .collect()
}

fn google_docs_portable_checkpoint(root_id: &RemoteId) -> PortableCheckpoint {
    PortableCheckpoint {
        format_version: GOOGLE_DOCS_PORTABLE_CHECKPOINT_VERSION,
        opaque: serde_json::json!({ "scope_root": root_id.as_str() }).to_string(),
    }
}

fn ensure_drive_file_within_root(
    drive: &dyn GoogleDriveApi,
    file: &DriveFile,
    root_id: &RemoteId,
) -> LocalityResult<()> {
    let mut parents = file.parents.clone();
    let mut visited = BTreeSet::new();
    while let Some(parent_id) = parents.pop() {
        if parent_id == root_id.as_str() {
            return Ok(());
        }
        if !visited.insert(parent_id.clone()) {
            continue;
        }
        let parent = drive.get_file(&parent_id)?;
        if parent.id != parent_id {
            return Err(LocalityError::InvalidState(format!(
                "Google Docs portable fetch resolved parent `{parent_id}` to different Drive id `{}`",
                parent.id
            )));
        }
        if parent.trashed || !parent.is_folder() {
            return Err(LocalityError::Guardrail(format!(
                "Google Docs portable fetch cannot prove `{}` is inside configured root `{}`",
                file.id,
                root_id.as_str()
            )));
        }
        parents.extend(parent.parents);
    }
    Err(LocalityError::Guardrail(format!(
        "Google Docs portable fetch cannot prove `{}` is inside configured root `{}`",
        file.id,
        root_id.as_str()
    )))
}

fn google_docs_artifact_key(
    remote_id: &RemoteId,
    role: &str,
    format_version: u32,
) -> PortableArtifactKey {
    PortableArtifactKey::new(format!(
        "google-docs:source:{}:{role}:v{format_version}",
        remote_id.as_str()
    ))
}
