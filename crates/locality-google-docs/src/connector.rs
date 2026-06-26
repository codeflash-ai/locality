use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest, ListChildrenRequest,
    ListChildrenResult, NativeEntity, ObserveRequest, ParsedEntity,
};
use locality_core::freshness::{RemoteObservation, RemoteVersion};
use locality_core::journal::JournalApplyEffect;
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, RemoteId, TreeEntry};
use locality_core::path_projection::{page_container_path, page_document_path};
use locality_core::planner::{PropertyValue, PushOperation, PushOperationKind, PushPlan};
use locality_core::{LocalityError, LocalityResult};

use crate::client::{GoogleDocsApi, GoogleDriveApi, HttpGoogleApiClient};
use crate::docs_dto::{
    BatchUpdateDocumentRequest, CreateParagraphBulletsRequest, DeleteContentRangeRequest,
    DeleteParagraphBulletsRequest, DocsRequest, GoogleDocument, InsertTextRequest, Link, Location,
    ParagraphStylePatch, Range, TextStyle, TextStylePatch, UpdateParagraphStyleRequest,
    UpdateTextStyleRequest, WriteControl,
};
use crate::drive_dto::{
    DRIVE_FOLDER_MIME_TYPE, DRIVE_GOOGLE_DOC_MIME_TYPE, DriveCreateFileRequest, DriveFile,
    DriveUpdateFileRequest,
};
use crate::oauth::GOOGLE_DOCS_CONNECTOR_ID;
use crate::render::{
    GoogleDocsNativeBundle, combined_remote_version, document_frontmatter, render_google_document,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleDocsConfig {
    pub access_token: String,
    pub workspace_folder_id: Option<RemoteId>,
}

impl GoogleDocsConfig {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            workspace_folder_id: None,
        }
    }

    pub fn with_workspace_folder_id(mut self, workspace_folder_id: RemoteId) -> Self {
        self.workspace_folder_id = Some(workspace_folder_id);
        self
    }
}

#[derive(Clone)]
pub struct GoogleDocsConnector {
    config: GoogleDocsConfig,
    drive: Arc<dyn GoogleDriveApi>,
    docs: Arc<dyn GoogleDocsApi>,
}

impl std::fmt::Debug for GoogleDocsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleDocsConnector")
            .field("workspace_folder_id", &self.config.workspace_folder_id)
            .field("access_token", &"<redacted>")
            .finish()
    }
}

impl GoogleDocsConnector {
    pub fn new(config: GoogleDocsConfig) -> Self {
        let api = Arc::new(HttpGoogleApiClient::new(config.access_token.clone()));
        Self::with_apis(config, api.clone(), api)
    }

    pub fn with_apis(
        config: GoogleDocsConfig,
        drive: Arc<dyn GoogleDriveApi>,
        docs: Arc<dyn GoogleDocsApi>,
    ) -> Self {
        Self {
            config,
            drive,
            docs,
        }
    }

    pub fn config(&self) -> &GoogleDocsConfig {
        &self.config
    }

    pub fn with_workspace_folder_id(&self, workspace_folder_id: RemoteId) -> Self {
        let mut config = self.config.clone();
        config.workspace_folder_id = Some(workspace_folder_id);
        Self {
            config,
            drive: Arc::clone(&self.drive),
            docs: Arc::clone(&self.docs),
        }
    }

    pub fn resolve_workspace_folder(&self, workspace_folder: &str) -> LocalityResult<RemoteId> {
        let workspace_folder = workspace_folder.trim();
        if workspace_folder.is_empty() {
            return Err(LocalityError::InvalidState(
                "google docs workspace folder cannot be empty".to_string(),
            ));
        }

        if let Some(folder_id) = extract_google_drive_folder_id(workspace_folder) {
            return self.verify_workspace_folder_id(&folder_id);
        }
        if looks_like_google_drive_id(workspace_folder) {
            match self.verify_workspace_folder_id(workspace_folder) {
                Ok(folder_id) => return Ok(folder_id),
                Err(LocalityError::RemoteNotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }

        if let Some(folder) = self.find_workspace_folder_by_name(workspace_folder)? {
            return Ok(RemoteId::new(folder.id));
        }
        let created = self
            .drive
            .create_file(DriveCreateFileRequest::folder(workspace_folder, None))?;
        if !created.is_folder() || created.trashed {
            return Err(LocalityError::InvalidState(format!(
                "google docs workspace folder create returned non-folder `{}`",
                created.id
            )));
        }
        Ok(RemoteId::new(created.id))
    }

    fn verify_workspace_folder_id(&self, folder_id: &str) -> LocalityResult<RemoteId> {
        let file = self.drive.get_file(folder_id)?;
        if file.trashed {
            return Err(LocalityError::RemoteNotFound(format!(
                "google docs workspace folder `{folder_id}` is trashed"
            )));
        }
        if !file.is_folder() {
            return Err(LocalityError::Guardrail(format!(
                "google docs workspace root `{folder_id}` is not a Google Drive folder"
            )));
        }
        Ok(RemoteId::new(file.id))
    }

    fn find_workspace_folder_by_name(&self, name: &str) -> LocalityResult<Option<DriveFile>> {
        let mut cursor = None;
        let mut matches = Vec::new();
        loop {
            let page = self
                .drive
                .list_workspace_folders_by_name(name, cursor.as_deref())?;
            matches.extend(
                page.files
                    .into_iter()
                    .filter(|file| file.is_folder() && !file.trashed),
            );
            if page.next_page_token.is_none() {
                break;
            }
            cursor = page.next_page_token;
        }
        matches.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(matches.into_iter().next())
    }
}

impl Connector for GoogleDocsConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind(GOOGLE_DOCS_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: true,
            supports_databases: false,
            supports_oauth: true,
            supports_remote_observation: true,
            supports_lazy_child_enumeration: true,
            supports_media_download: false,
            supports_undo: false,
            supports_batch_observation: false,
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        [
            PushOperationKind::UpdateBlock,
            PushOperationKind::ReplaceBlock,
            PushOperationKind::AppendBlock,
            PushOperationKind::ArchiveBlock,
            PushOperationKind::ArchiveEntity,
            PushOperationKind::UpdateProperties,
            PushOperationKind::CreateEntity,
        ]
        .into_iter()
        .collect()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        let root_id = self.workspace_folder_id()?;
        enumerate_drive_tree(
            self.drive.as_ref(),
            &request.mount_id,
            root_id.as_str(),
            Path::new(""),
        )
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let parent_id = match request.container {
            locality_connector::ChildContainer::Root => self.workspace_folder_id()?.0.clone(),
            locality_connector::ChildContainer::PageChildren(remote_id)
            | locality_connector::ChildContainer::DatabaseRows(remote_id) => remote_id.0,
        };
        Ok(ListChildrenResult {
            entries: list_drive_children(
                self.drive.as_ref(),
                &request.mount_id,
                &parent_id,
                &request.parent_path,
            )?,
        })
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        let file = self.drive.get_file(request.remote_id.as_str())?;
        let revision = if file.is_google_doc() {
            self.docs
                .get_document(file.id.as_str())?
                .revision_id
                .unwrap_or_default()
        } else {
            String::new()
        };
        let kind = if file.is_folder() {
            EntityKind::Directory
        } else if file.is_google_doc() {
            EntityKind::Page
        } else {
            EntityKind::Unknown(file.mime_type.clone())
        };
        let projected_path = if file.is_google_doc() {
            page_document_path(Path::new(&slugify_title(&file.name)))
        } else {
            PathBuf::from(slugify_title(&file.name))
        };
        let mut observation = RemoteObservation::new(
            request.mount_id,
            RemoteId::new(file.id.clone()),
            kind,
            file.name.clone(),
            projected_path,
        )
        .deleted(file.trashed)
        .with_raw_metadata_json(serde_json::to_string(&file).unwrap_or_else(|_| "{}".to_string()));
        if let Some(parent) = file.parents.first() {
            observation = observation.with_parent(RemoteId::new(parent.clone()));
        }
        observation = observation.with_remote_version(RemoteVersion::new(combined_remote_version(
            &file,
            Some(revision.as_str()),
        )));
        Ok(observation)
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        let drive_file = self.drive.get_file(request.remote_id.as_str())?;
        if !drive_file.is_google_doc() {
            return Err(LocalityError::Unsupported(
                "google docs connector only hydrates Google Docs files",
            ));
        }
        let document = self.docs.get_document(request.remote_id.as_str())?;
        let bundle = GoogleDocsNativeBundle {
            drive_file,
            document,
        };
        let raw = serde_json::to_vec(&bundle).map_err(|error| {
            LocalityError::Io(format!("google docs native encode failed: {error}"))
        })?;
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: "google_docs_document".to_string(),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        let bundle =
            serde_json::from_slice::<GoogleDocsNativeBundle>(&entity.raw).map_err(|error| {
                LocalityError::Io(format!("google docs native decode failed: {error}"))
            })?;
        render_google_document(&bundle).map(|rendered| rendered.document)
    }

    fn parse(&self, document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        if document.body.contains("type=google_docs_unsupported") {
            return Err(LocalityError::Unsupported(
                "google docs document contains unsupported push-blocking directives",
            ));
        }
        Err(LocalityError::NotImplemented("google docs parse"))
    }

    fn check_concurrency(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        for precondition in request.remote_preconditions {
            let Some(expected) = &precondition.remote_edited_at else {
                continue;
            };
            let current = self.remote_version(&precondition.remote_id)?;
            if expected == current.as_str() {
                continue;
            }
            if docs_revision_matches(expected, current.as_str())
                && plan_changes_only_document_body(request.plan, &precondition.remote_id)
            {
                continue;
            }
            return Err(LocalityError::Conflict(
                locality_core::conflict::ConflictSummary {
                    remote_id: precondition.remote_id.clone(),
                    path: PathBuf::from(precondition.remote_id.as_str()),
                    remote_path: PathBuf::from(precondition.remote_id.as_str()),
                    reason: locality_core::conflict::ConflictReason::RemoteMovedDuringPush,
                },
            ));
        }
        Ok(())
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        apply_plan(self.drive.as_ref(), self.docs.as_ref(), request)
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::Unsupported("google docs undo"))
    }
}

fn docs_revision_matches(expected: &str, current: &str) -> bool {
    match (
        docs_revision_from_remote_version(expected),
        docs_revision_from_remote_version(current),
    ) {
        (Some(expected), Some(current)) => expected == current,
        _ => false,
    }
}

fn docs_revision_from_remote_version(version: &str) -> Option<&str> {
    version
        .rsplit_once("|docs:")
        .map(|(_, revision)| revision)
        .or_else(|| version.strip_prefix("docs:"))
}

fn plan_changes_only_document_body(plan: &PushPlan, remote_id: &RemoteId) -> bool {
    let mut body_change = false;
    for operation in &plan.operations {
        match operation {
            PushOperation::UpdateBlock { block_id, .. }
            | PushOperation::ReplaceBlock { block_id, .. }
            | PushOperation::ArchiveBlock { block_id } => {
                if operation_targets_document(block_id, remote_id) {
                    body_change = true;
                }
            }
            PushOperation::AppendBlock { parent_id, .. } if parent_id == remote_id => {
                body_change = true;
            }
            PushOperation::UpdateMedia { block_id, .. }
            | PushOperation::MoveBlock { block_id, .. }
                if operation_targets_document(block_id, remote_id) =>
            {
                return false;
            }
            PushOperation::UpdateProperties { entity_id, .. }
            | PushOperation::ArchiveEntity { entity_id }
                if entity_id == remote_id =>
            {
                return false;
            }
            PushOperation::CreateEntity { parent_id, .. } if parent_id == remote_id => {
                return false;
            }
            _ => {}
        }
    }
    body_change
}

fn operation_targets_document(block_id: &RemoteId, remote_id: &RemoteId) -> bool {
    GoogleBlockRange::parse(block_id)
        .map(|range| range.document_id == remote_id.0)
        .unwrap_or(false)
}

impl GoogleDocsConnector {
    fn workspace_folder_id(&self) -> LocalityResult<&RemoteId> {
        self.config.workspace_folder_id.as_ref().ok_or_else(|| {
            LocalityError::InvalidState(
                "google docs mount is missing workspace folder id".to_string(),
            )
        })
    }

    fn remote_version(&self, remote_id: &RemoteId) -> LocalityResult<String> {
        let file = self.drive.get_file(remote_id.as_str())?;
        let revision = if file.is_google_doc() {
            self.docs.get_document(remote_id.as_str())?.revision_id
        } else {
            None
        };
        Ok(combined_remote_version(&file, revision.as_deref()))
    }
}

fn enumerate_drive_tree(
    drive: &dyn GoogleDriveApi,
    mount_id: &locality_core::model::MountId,
    parent_id: &str,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let children = list_drive_children(drive, mount_id, parent_id, parent_path)?;
    for entry in children {
        let is_directory = entry.kind == EntityKind::Directory;
        let remote_id = entry.remote_id.clone();
        let dir_path = entry.path.clone();
        entries.push(entry);
        if is_directory {
            entries.extend(enumerate_drive_tree(
                drive,
                mount_id,
                remote_id.as_str(),
                &dir_path,
            )?);
        }
    }
    Ok(entries)
}

fn list_drive_children(
    drive: &dyn GoogleDriveApi,
    mount_id: &locality_core::model::MountId,
    parent_id: &str,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let mut cursor = None;
    let mut files = Vec::new();
    loop {
        let page = drive.list_children(parent_id, cursor.as_deref())?;
        files.extend(page.files.into_iter().filter(|file| !file.trashed));
        if page.next_page_token.is_none() {
            break;
        }
        cursor = page.next_page_token;
    }
    files.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(project_drive_children(mount_id, parent_path, files))
}

fn project_drive_children(
    mount_id: &locality_core::model::MountId,
    parent_path: &Path,
    files: Vec<DriveFile>,
) -> Vec<TreeEntry> {
    let mut used_paths = BTreeSet::new();
    files
        .into_iter()
        .filter(|file| file.is_folder() || file.is_google_doc())
        .map(|file| {
            let path = if file.is_folder() {
                allocate_path(parent_path, &file.name, &file.id, false, &mut used_paths)
            } else {
                allocate_path(parent_path, &file.name, &file.id, true, &mut used_paths)
            };
            let remote_version = file.remote_version();
            let stub_frontmatter = if file.is_google_doc() {
                Some(document_frontmatter(&file, ""))
            } else {
                None
            };
            TreeEntry {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new(file.id),
                kind: if file.mime_type == DRIVE_FOLDER_MIME_TYPE {
                    EntityKind::Directory
                } else if file.mime_type == DRIVE_GOOGLE_DOC_MIME_TYPE {
                    EntityKind::Page
                } else {
                    EntityKind::Unknown(file.mime_type)
                },
                title: file.name,
                path,
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: remote_version,
                stub_frontmatter,
            }
        })
        .collect()
}

fn apply_plan(
    drive: &dyn GoogleDriveApi,
    docs: &dyn GoogleDocsApi,
    request: ApplyPlanRequest<'_>,
) -> LocalityResult<ApplyPlanResult> {
    let mut changed = BTreeSet::new();
    let mut effects = Vec::new();
    let mut append_offsets: BTreeMap<(String, Option<String>), usize> = BTreeMap::new();
    let mut inserted_ranges: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    for index in apply_operation_order(&request.plan.operations) {
        let operation = &request.plan.operations[index];
        let operation_id = request
            .operation_ids
            .get(index)
            .cloned()
            .ok_or_else(|| LocalityError::InvalidState("missing operation id".to_string()))?;
        match operation {
            PushOperation::UpdateBlock { block_id, content }
            | PushOperation::ReplaceBlock { block_id, content } => {
                let range = GoogleBlockRange::parse(block_id)?;
                let range = range.shifted_for_insertions(
                    inserted_ranges
                        .get(&range.document_id)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                );
                let document = docs.get_document(&range.document_id)?;
                let final_block = range.end_index == document_end_index(&document);
                let delete_end_index = if final_block && range.end_index > range.start_index {
                    range.end_index - 1
                } else {
                    range.end_index
                };
                let mut requests = vec![DocsRequest::DeleteContentRange {
                    delete_content_range: DeleteContentRangeRequest {
                        range: Range {
                            start_index: range.start_index,
                            end_index: delete_end_index,
                        },
                    },
                }];
                let mut docs_text = docs_block_text(content);
                if final_block {
                    strip_trailing_segment_newline(&mut docs_text);
                }
                requests.extend(docs_text_requests_from_parsed(
                    range.start_index,
                    docs_text,
                    Some(DocsTextStyleSource {
                        document: &document,
                        start_index: range.start_index,
                        end_index: delete_end_index,
                    }),
                ));
                docs.batch_update_document(
                    &range.document_id,
                    BatchUpdateDocumentRequest {
                        requests,
                        write_control: write_control(&document),
                    },
                )?;
                let remote_id = RemoteId::new(range.document_id);
                changed.insert(remote_id.clone());
                effects.push(JournalApplyEffect::UpdatedBlock {
                    operation_id,
                    operation_index: index,
                    block_id: block_id.clone(),
                });
            }
            PushOperation::AppendBlock {
                parent_id,
                after,
                content,
            } => {
                let document = docs.get_document(parent_id.as_str())?;
                let base_index = after
                    .as_ref()
                    .and_then(|after| GoogleBlockRange::parse(after).ok())
                    .map(|range| {
                        shift_index_for_insertions(
                            range.end_index,
                            inserted_ranges
                                .get(&range.document_id)
                                .map(Vec::as_slice)
                                .unwrap_or(&[]),
                        )
                    })
                    .unwrap_or_else(|| document_start_index(&document));
                let append_key = (
                    parent_id.0.clone(),
                    after.as_ref().map(|remote_id| remote_id.0.clone()),
                );
                let index_position =
                    base_index + append_offsets.get(&append_key).copied().unwrap_or_default();
                let docs_text = docs_block_text(content);
                let inserted_len = docs_text_len(&docs_text.text);
                let new_block_end = index_position + inserted_len;
                let requests = docs_text_requests_from_parsed(index_position, docs_text, None);
                docs.batch_update_document(
                    parent_id.as_str(),
                    BatchUpdateDocumentRequest {
                        requests,
                        write_control: write_control(&document),
                    },
                )?;
                *append_offsets.entry(append_key).or_default() += inserted_len;
                inserted_ranges
                    .entry(parent_id.0.clone())
                    .or_default()
                    .push((index_position, inserted_len));
                changed.insert(parent_id.clone());
                effects.push(JournalApplyEffect::CreatedBlock {
                    operation_id,
                    operation_index: index,
                    parent_id: parent_id.clone(),
                    block_id: RemoteId::new(format!(
                        "{}:{}:{}",
                        parent_id.0, index_position, new_block_end
                    )),
                });
            }
            PushOperation::ArchiveBlock { block_id } => {
                let range = GoogleBlockRange::parse(block_id)?;
                let range = range.shifted_for_insertions(
                    inserted_ranges
                        .get(&range.document_id)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                );
                let document = docs.get_document(&range.document_id)?;
                docs.batch_update_document(
                    &range.document_id,
                    BatchUpdateDocumentRequest {
                        requests: vec![DocsRequest::DeleteContentRange {
                            delete_content_range: DeleteContentRangeRequest {
                                range: Range {
                                    start_index: range.start_index,
                                    end_index: range.end_index,
                                },
                            },
                        }],
                        write_control: write_control(&document),
                    },
                )?;
                let remote_id = RemoteId::new(range.document_id);
                changed.insert(remote_id.clone());
                effects.push(JournalApplyEffect::ArchivedBlock {
                    operation_id,
                    operation_index: index,
                    block_id: block_id.clone(),
                });
            }
            PushOperation::ArchiveEntity { entity_id } => {
                drive.update_file(entity_id.as_str(), DriveUpdateFileRequest::trash())?;
                changed.insert(entity_id.clone());
                effects.push(JournalApplyEffect::ArchivedEntity {
                    operation_id,
                    operation_index: index,
                    entity_id: entity_id.clone(),
                });
            }
            PushOperation::UpdateProperties {
                entity_id,
                properties,
            } => {
                if let Some(PropertyValue::String(title)) = properties.get("title") {
                    drive.update_file(entity_id.as_str(), DriveUpdateFileRequest::rename(title))?;
                    changed.insert(entity_id.clone());
                    effects.push(JournalApplyEffect::UpdatedProperties {
                        operation_id,
                        operation_index: index,
                        entity_id: entity_id.clone(),
                        keys: vec!["title".to_string()],
                    });
                }
            }
            PushOperation::CreateEntity {
                parent_id,
                title,
                body,
                ..
            } => {
                let created = drive.create_file(DriveCreateFileRequest::google_doc(
                    title,
                    parent_id.0.clone(),
                ))?;
                if !body.trim().is_empty() {
                    let document = docs
                        .get_document(created.id.as_str())
                        .unwrap_or_else(|_| empty_document(created.id.as_str(), title));
                    if let Err(error) = docs.batch_update_document(
                        created.id.as_str(),
                        BatchUpdateDocumentRequest {
                            requests: docs_document_text_requests(1, body),
                            write_control: write_control(&document),
                        },
                    ) {
                        let _ =
                            drive.update_file(created.id.as_str(), DriveUpdateFileRequest::trash());
                        return Err(error);
                    }
                }
                let entity_id = RemoteId::new(created.id);
                changed.insert(entity_id.clone());
                effects.push(JournalApplyEffect::CreatedEntity {
                    operation_id,
                    operation_index: index,
                    parent_id: parent_id.clone(),
                    entity_id,
                });
            }
            PushOperation::MoveBlock { .. } | PushOperation::UpdateMedia { .. } => {
                return Err(LocalityError::Unsupported(
                    "google docs connector cannot apply this operation",
                ));
            }
        }
    }
    Ok(ApplyPlanResult {
        changed_remote_ids: changed.into_iter().collect(),
        effects,
    })
}

fn apply_operation_order(operations: &[PushOperation]) -> Vec<usize> {
    let mut order = Vec::with_capacity(operations.len());
    let mut index = 0;
    while index < operations.len() {
        let Some(first_range) = operation_block_range(&operations[index]) else {
            order.push(index);
            index += 1;
            continue;
        };

        let document_id = first_range.document_id;
        let mut group = Vec::new();
        while index < operations.len() {
            let Some(range) = operation_block_range(&operations[index]) else {
                break;
            };
            if range.document_id != document_id {
                break;
            }
            group.push((index, range.start_index));
            index += 1;
        }
        group.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        order.extend(group.into_iter().map(|(index, _)| index));
    }
    order
}

fn operation_block_range(operation: &PushOperation) -> Option<GoogleBlockRange> {
    match operation {
        PushOperation::UpdateBlock { block_id, .. }
        | PushOperation::ReplaceBlock { block_id, .. }
        | PushOperation::ArchiveBlock { block_id } => GoogleBlockRange::parse(block_id).ok(),
        _ => None,
    }
}

fn write_control(document: &GoogleDocument) -> Option<WriteControl> {
    Some(WriteControl {
        required_revision_id: document.revision_id.clone(),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DocsText {
    text: String,
    style_ranges: Vec<DocsTextStyleRange>,
    paragraph_styles: Vec<DocsParagraphStyleRange>,
    bullet_ranges: Vec<DocsBulletRange>,
    list_block: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocsTextStyleRange {
    start: usize,
    end: usize,
    style: DocsInlineStyle,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DocsInlineStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    link: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocsParagraphStyleRange {
    start: usize,
    end: usize,
    named_style_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocsParagraphAlignmentRange {
    start: usize,
    end: usize,
    alignment: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocsParagraphIndentRange {
    start: usize,
    end: usize,
    indent_start: Option<serde_json::Value>,
    indent_first_line: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocsBulletRange {
    start: usize,
    end: usize,
    preset: String,
}

fn docs_document_text_requests(location_index: usize, content: &str) -> Vec<DocsRequest> {
    docs_text_requests_from_parsed(location_index, docs_document_text(content), None)
}

fn docs_text_requests_from_parsed(
    location_index: usize,
    docs_text: DocsText,
    style_source: Option<DocsTextStyleSource<'_>>,
) -> Vec<DocsRequest> {
    let inserted_len = docs_text_len(&docs_text.text);
    let inserted_text = docs_text.text.clone();
    let preserved_color_ranges = style_source
        .map(|source| preserved_color_ranges(&inserted_text, source, &docs_text.style_ranges))
        .unwrap_or_default();
    let preserved_background_ranges = style_source
        .map(|source| preserved_background_ranges(&inserted_text, source, &docs_text.style_ranges))
        .unwrap_or_default();
    let preserved_baseline_ranges = style_source
        .map(|source| preserved_baseline_ranges(&inserted_text, source, &docs_text.style_ranges))
        .unwrap_or_default();
    let preserved_font_size_ranges = style_source
        .map(|source| preserved_font_size_ranges(&inserted_text, source, &docs_text.style_ranges))
        .unwrap_or_default();
    let preserved_font_family_ranges = style_source
        .map(|source| preserved_font_family_ranges(&inserted_text, source, &docs_text.style_ranges))
        .unwrap_or_default();
    let preserved_small_caps_ranges = style_source
        .map(|source| preserved_small_caps_ranges(&inserted_text, source, &docs_text.style_ranges))
        .unwrap_or_default();
    let preserved_paragraph_alignments = style_source
        .map(|source| preserved_paragraph_alignments(&inserted_text, source))
        .unwrap_or_default();
    let preserved_paragraph_indents = style_source
        .map(|source| preserved_paragraph_indents(&inserted_text, source))
        .unwrap_or_default();
    let mut requests = vec![DocsRequest::InsertText {
        insert_text: InsertTextRequest {
            location: Location {
                index: location_index,
            },
            text: docs_text.text,
        },
    }];
    if inserted_len > 0 {
        requests.push(reset_text_style_request(
            location_index,
            location_index + inserted_len,
        ));
        if !docs_text.list_block {
            requests.push(delete_paragraph_bullets_request(
                location_index,
                location_index + inserted_len,
            ));
        }
    }
    requests.extend(
        docs_text
            .paragraph_styles
            .into_iter()
            .map(|range| paragraph_style_request(location_index, range)),
    );
    requests.extend(
        preserved_paragraph_alignments
            .into_iter()
            .map(|range| paragraph_alignment_request(location_index, range)),
    );
    requests.extend(
        preserved_paragraph_indents
            .into_iter()
            .map(|range| paragraph_indent_request(location_index, range)),
    );
    requests.extend(
        preserved_color_ranges
            .into_iter()
            .map(|range| foreground_color_request(location_index, range)),
    );
    requests.extend(
        preserved_background_ranges
            .into_iter()
            .map(|range| background_color_request(location_index, range)),
    );
    requests.extend(
        preserved_baseline_ranges
            .into_iter()
            .map(|range| baseline_offset_request(location_index, range)),
    );
    requests.extend(
        preserved_font_size_ranges
            .into_iter()
            .map(|range| font_size_request(location_index, range)),
    );
    requests.extend(
        preserved_font_family_ranges
            .into_iter()
            .map(|range| font_family_request(location_index, range)),
    );
    requests.extend(
        preserved_small_caps_ranges
            .into_iter()
            .map(|range| small_caps_request(location_index, range)),
    );
    requests.extend(
        docs_text
            .style_ranges
            .into_iter()
            .map(|range| text_style_request(location_index, range, style_source)),
    );
    requests.extend(
        merge_adjacent_bullet_ranges(docs_text.bullet_ranges)
            .into_iter()
            .map(|range| create_paragraph_bullets_request(location_index, range)),
    );
    requests
}

fn merge_adjacent_bullet_ranges(ranges: Vec<DocsBulletRange>) -> Vec<DocsBulletRange> {
    let mut merged: Vec<DocsBulletRange> = Vec::new();
    for range in ranges {
        if let Some(previous) = merged.last_mut()
            && previous.preset == range.preset
            && previous.end == range.start
        {
            previous.end = range.end;
            continue;
        }
        merged.push(range);
    }
    merged
}

#[derive(Clone, Copy)]
struct DocsTextStyleSource<'a> {
    document: &'a GoogleDocument,
    start_index: usize,
    end_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocsForegroundColorRange {
    start: usize,
    end: usize,
    foreground_color: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocsBackgroundColorRange {
    start: usize,
    end: usize,
    background_color: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocsBaselineOffsetRange {
    start: usize,
    end: usize,
    baseline_offset: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocsFontSizeRange {
    start: usize,
    end: usize,
    font_size: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocsFontFamilyRange {
    start: usize,
    end: usize,
    weighted_font_family: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocsSmallCapsRange {
    start: usize,
    end: usize,
}

fn reset_text_style_request(start_index: usize, end_index: usize) -> DocsRequest {
    DocsRequest::UpdateTextStyle {
        update_text_style: UpdateTextStyleRequest {
            range: Range {
                start_index,
                end_index,
            },
            text_style: TextStylePatch {
                bold: Some(false),
                italic: Some(false),
                underline: Some(false),
                strikethrough: Some(false),
                small_caps: Some(false),
                foreground_color: None,
                background_color: None,
                baseline_offset: Some("NONE".to_string()),
                font_size: None,
                weighted_font_family: None,
                link: None,
            },
            fields:
                "bold,italic,underline,strikethrough,smallCaps,foregroundColor,backgroundColor,baselineOffset,fontSize,weightedFontFamily,link"
                    .to_string(),
        },
    }
}

fn delete_paragraph_bullets_request(start_index: usize, end_index: usize) -> DocsRequest {
    DocsRequest::DeleteParagraphBullets {
        delete_paragraph_bullets: DeleteParagraphBulletsRequest {
            range: Range {
                start_index,
                end_index,
            },
        },
    }
}

fn paragraph_style_request(location_index: usize, range: DocsParagraphStyleRange) -> DocsRequest {
    DocsRequest::UpdateParagraphStyle {
        update_paragraph_style: UpdateParagraphStyleRequest {
            range: Range {
                start_index: location_index + range.start,
                end_index: location_index + range.end,
            },
            paragraph_style: ParagraphStylePatch {
                named_style_type: Some(range.named_style_type),
                ..ParagraphStylePatch::default()
            },
            fields: "namedStyleType".to_string(),
        },
    }
}

fn paragraph_alignment_request(
    location_index: usize,
    range: DocsParagraphAlignmentRange,
) -> DocsRequest {
    DocsRequest::UpdateParagraphStyle {
        update_paragraph_style: UpdateParagraphStyleRequest {
            range: Range {
                start_index: location_index + range.start,
                end_index: location_index + range.end,
            },
            paragraph_style: ParagraphStylePatch {
                alignment: Some(range.alignment),
                ..ParagraphStylePatch::default()
            },
            fields: "alignment".to_string(),
        },
    }
}

fn paragraph_indent_request(location_index: usize, range: DocsParagraphIndentRange) -> DocsRequest {
    let mut fields = Vec::new();
    if range.indent_start.is_some() {
        fields.push("indentStart");
    }
    if range.indent_first_line.is_some() {
        fields.push("indentFirstLine");
    }
    DocsRequest::UpdateParagraphStyle {
        update_paragraph_style: UpdateParagraphStyleRequest {
            range: Range {
                start_index: location_index + range.start,
                end_index: location_index + range.end,
            },
            paragraph_style: ParagraphStylePatch {
                indent_start: range.indent_start,
                indent_first_line: range.indent_first_line,
                ..ParagraphStylePatch::default()
            },
            fields: fields.join(","),
        },
    }
}

fn create_paragraph_bullets_request(location_index: usize, range: DocsBulletRange) -> DocsRequest {
    DocsRequest::CreateParagraphBullets {
        create_paragraph_bullets: CreateParagraphBulletsRequest {
            range: Range {
                start_index: location_index + range.start,
                end_index: location_index + range.end,
            },
            bullet_preset: range.preset,
        },
    }
}

fn foreground_color_request(location_index: usize, range: DocsForegroundColorRange) -> DocsRequest {
    DocsRequest::UpdateTextStyle {
        update_text_style: UpdateTextStyleRequest {
            range: Range {
                start_index: location_index + range.start,
                end_index: location_index + range.end,
            },
            text_style: TextStylePatch {
                foreground_color: Some(range.foreground_color),
                ..TextStylePatch::default()
            },
            fields: "foregroundColor".to_string(),
        },
    }
}

fn background_color_request(location_index: usize, range: DocsBackgroundColorRange) -> DocsRequest {
    DocsRequest::UpdateTextStyle {
        update_text_style: UpdateTextStyleRequest {
            range: Range {
                start_index: location_index + range.start,
                end_index: location_index + range.end,
            },
            text_style: TextStylePatch {
                background_color: Some(range.background_color),
                ..TextStylePatch::default()
            },
            fields: "backgroundColor".to_string(),
        },
    }
}

fn baseline_offset_request(location_index: usize, range: DocsBaselineOffsetRange) -> DocsRequest {
    DocsRequest::UpdateTextStyle {
        update_text_style: UpdateTextStyleRequest {
            range: Range {
                start_index: location_index + range.start,
                end_index: location_index + range.end,
            },
            text_style: TextStylePatch {
                baseline_offset: Some(range.baseline_offset),
                ..TextStylePatch::default()
            },
            fields: "baselineOffset".to_string(),
        },
    }
}

fn font_size_request(location_index: usize, range: DocsFontSizeRange) -> DocsRequest {
    DocsRequest::UpdateTextStyle {
        update_text_style: UpdateTextStyleRequest {
            range: Range {
                start_index: location_index + range.start,
                end_index: location_index + range.end,
            },
            text_style: TextStylePatch {
                font_size: Some(range.font_size),
                ..TextStylePatch::default()
            },
            fields: "fontSize".to_string(),
        },
    }
}

fn font_family_request(location_index: usize, range: DocsFontFamilyRange) -> DocsRequest {
    DocsRequest::UpdateTextStyle {
        update_text_style: UpdateTextStyleRequest {
            range: Range {
                start_index: location_index + range.start,
                end_index: location_index + range.end,
            },
            text_style: TextStylePatch {
                weighted_font_family: Some(range.weighted_font_family),
                ..TextStylePatch::default()
            },
            fields: "weightedFontFamily".to_string(),
        },
    }
}

fn small_caps_request(location_index: usize, range: DocsSmallCapsRange) -> DocsRequest {
    DocsRequest::UpdateTextStyle {
        update_text_style: UpdateTextStyleRequest {
            range: Range {
                start_index: location_index + range.start,
                end_index: location_index + range.end,
            },
            text_style: TextStylePatch {
                small_caps: Some(true),
                ..TextStylePatch::default()
            },
            fields: "smallCaps".to_string(),
        },
    }
}

fn text_style_request(
    location_index: usize,
    range: DocsTextStyleRange,
    style_source: Option<DocsTextStyleSource<'_>>,
) -> DocsRequest {
    let existing_style = style_source
        .and_then(|source| text_style_at(source.document, source.start_index + range.start));
    let foreground_color = existing_style
        .filter(|_| {
            range.style.bold
                || range.style.italic
                || range.style.underline
                || range.style.strikethrough
                || range.style.link.is_some()
        })
        .and_then(|style| style.foreground_color.clone());
    let background_color = existing_style
        .filter(|_| {
            range.style.bold
                || range.style.italic
                || range.style.underline
                || range.style.strikethrough
                || range.style.link.is_some()
        })
        .and_then(|style| style.background_color.clone());
    let baseline_offset = existing_style
        .filter(|_| {
            range.style.bold
                || range.style.italic
                || range.style.underline
                || range.style.strikethrough
                || range.style.link.is_some()
        })
        .and_then(|style| style.baseline_offset.clone())
        .filter(|baseline_offset| baseline_offset != "NONE");
    let font_size = existing_style
        .filter(|_| {
            range.style.bold
                || range.style.italic
                || range.style.underline
                || range.style.strikethrough
                || range.style.link.is_some()
        })
        .and_then(|style| style.font_size.clone());
    let weighted_font_family = existing_style
        .filter(|_| {
            range.style.bold
                || range.style.italic
                || range.style.underline
                || range.style.strikethrough
                || range.style.link.is_some()
        })
        .and_then(|style| style.weighted_font_family.clone());
    let small_caps = existing_style
        .filter(|_| {
            range.style.bold
                || range.style.italic
                || range.style.underline
                || range.style.strikethrough
                || range.style.link.is_some()
        })
        .and_then(|style| style.small_caps.then_some(true));
    let mut fields = Vec::new();
    if range.style.bold {
        fields.push("bold");
    }
    if range.style.italic {
        fields.push("italic");
    }
    if range.style.underline {
        fields.push("underline");
    }
    if range.style.strikethrough {
        fields.push("strikethrough");
    }
    if small_caps.is_some() {
        fields.push("smallCaps");
    }
    if range.style.link.is_some() {
        fields.push("link");
    }
    if foreground_color.is_some() {
        fields.push("foregroundColor");
    }
    if background_color.is_some() {
        fields.push("backgroundColor");
    }
    if baseline_offset.is_some() {
        fields.push("baselineOffset");
    }
    if font_size.is_some() {
        fields.push("fontSize");
    }
    if weighted_font_family.is_some() {
        fields.push("weightedFontFamily");
    }
    DocsRequest::UpdateTextStyle {
        update_text_style: UpdateTextStyleRequest {
            range: Range {
                start_index: location_index + range.start,
                end_index: location_index + range.end,
            },
            text_style: TextStylePatch {
                bold: range.style.bold.then_some(true),
                italic: range.style.italic.then_some(true),
                underline: range.style.underline.then_some(true),
                strikethrough: range.style.strikethrough.then_some(true),
                small_caps,
                foreground_color,
                background_color,
                baseline_offset,
                font_size,
                weighted_font_family,
                link: range.style.link.map(|url| Link { url: Some(url) }),
            },
            fields: fields.join(","),
        },
    }
}

fn preserved_color_ranges(
    new_text: &str,
    source: DocsTextStyleSource<'_>,
    explicit_style_ranges: &[DocsTextStyleRange],
) -> Vec<DocsForegroundColorRange> {
    let (source_text, source_ranges) = source_text_color_ranges(source);
    source_ranges
        .into_iter()
        .filter_map(|range| {
            let (start, end) =
                map_source_range_by_context(&source_text, range.start, range.end, new_text)?;
            if explicit_style_ranges
                .iter()
                .any(|explicit| ranges_overlap(start, end, explicit.start, explicit.end))
            {
                return None;
            }
            Some(DocsForegroundColorRange {
                start,
                end,
                foreground_color: range.foreground_color,
            })
        })
        .collect()
}

fn preserved_background_ranges(
    new_text: &str,
    source: DocsTextStyleSource<'_>,
    explicit_style_ranges: &[DocsTextStyleRange],
) -> Vec<DocsBackgroundColorRange> {
    let (source_text, source_ranges) = source_text_background_ranges(source);
    source_ranges
        .into_iter()
        .filter_map(|range| {
            let (start, end) =
                map_source_range_by_context(&source_text, range.start, range.end, new_text)?;
            if explicit_style_ranges
                .iter()
                .any(|explicit| ranges_overlap(start, end, explicit.start, explicit.end))
            {
                return None;
            }
            Some(DocsBackgroundColorRange {
                start,
                end,
                background_color: range.background_color,
            })
        })
        .collect()
}

fn preserved_baseline_ranges(
    new_text: &str,
    source: DocsTextStyleSource<'_>,
    explicit_style_ranges: &[DocsTextStyleRange],
) -> Vec<DocsBaselineOffsetRange> {
    let (source_text, source_ranges) = source_text_baseline_ranges(source);
    source_ranges
        .into_iter()
        .filter_map(|range| {
            let (start, end) =
                map_source_range_by_context(&source_text, range.start, range.end, new_text)?;
            if explicit_style_ranges
                .iter()
                .any(|explicit| ranges_overlap(start, end, explicit.start, explicit.end))
            {
                return None;
            }
            Some(DocsBaselineOffsetRange {
                start,
                end,
                baseline_offset: range.baseline_offset,
            })
        })
        .collect()
}

fn preserved_font_size_ranges(
    new_text: &str,
    source: DocsTextStyleSource<'_>,
    explicit_style_ranges: &[DocsTextStyleRange],
) -> Vec<DocsFontSizeRange> {
    let (source_text, source_ranges) = source_text_font_size_ranges(source);
    source_ranges
        .into_iter()
        .filter_map(|range| {
            let (start, end) =
                map_source_range_by_context(&source_text, range.start, range.end, new_text)?;
            if explicit_style_ranges
                .iter()
                .any(|explicit| ranges_overlap(start, end, explicit.start, explicit.end))
            {
                return None;
            }
            Some(DocsFontSizeRange {
                start,
                end,
                font_size: range.font_size,
            })
        })
        .collect()
}

fn preserved_font_family_ranges(
    new_text: &str,
    source: DocsTextStyleSource<'_>,
    explicit_style_ranges: &[DocsTextStyleRange],
) -> Vec<DocsFontFamilyRange> {
    let (source_text, source_ranges) = source_text_font_family_ranges(source);
    source_ranges
        .into_iter()
        .filter_map(|range| {
            let (start, end) =
                map_source_range_by_context(&source_text, range.start, range.end, new_text)?;
            if explicit_style_ranges
                .iter()
                .any(|explicit| ranges_overlap(start, end, explicit.start, explicit.end))
            {
                return None;
            }
            Some(DocsFontFamilyRange {
                start,
                end,
                weighted_font_family: range.weighted_font_family,
            })
        })
        .collect()
}

fn preserved_small_caps_ranges(
    new_text: &str,
    source: DocsTextStyleSource<'_>,
    explicit_style_ranges: &[DocsTextStyleRange],
) -> Vec<DocsSmallCapsRange> {
    let (source_text, source_ranges) = source_text_small_caps_ranges(source);
    source_ranges
        .into_iter()
        .filter_map(|range| {
            let (start, end) =
                map_source_range_by_context(&source_text, range.start, range.end, new_text)?;
            if explicit_style_ranges
                .iter()
                .any(|explicit| ranges_overlap(start, end, explicit.start, explicit.end))
            {
                return None;
            }
            Some(DocsSmallCapsRange { start, end })
        })
        .collect()
}

fn preserved_paragraph_alignments(
    new_text: &str,
    source: DocsTextStyleSource<'_>,
) -> Vec<DocsParagraphAlignmentRange> {
    let (source_text, source_ranges) = source_text_paragraph_alignment_ranges(source);
    source_ranges
        .into_iter()
        .filter_map(|range| {
            let (start, end) =
                map_paragraph_range_by_context(&source_text, range.start, range.end, new_text)?;
            Some(DocsParagraphAlignmentRange {
                start,
                end,
                alignment: range.alignment,
            })
        })
        .collect()
}

fn preserved_paragraph_indents(
    new_text: &str,
    source: DocsTextStyleSource<'_>,
) -> Vec<DocsParagraphIndentRange> {
    let (source_text, source_ranges) = source_text_paragraph_indent_ranges(source);
    source_ranges
        .into_iter()
        .filter_map(|range| {
            let (start, end) =
                map_paragraph_range_by_context(&source_text, range.start, range.end, new_text)?;
            Some(DocsParagraphIndentRange {
                start,
                end,
                indent_start: range.indent_start,
                indent_first_line: range.indent_first_line,
            })
        })
        .collect()
}

fn source_text_color_ranges(
    source: DocsTextStyleSource<'_>,
) -> (String, Vec<DocsForegroundColorRange>) {
    let mut source_text = String::new();
    let mut ranges = Vec::new();
    for element in source
        .document
        .body
        .content
        .iter()
        .filter_map(|element| element.paragraph.as_ref())
        .flat_map(|paragraph| paragraph.elements.iter())
    {
        let (Some(element_start), Some(element_end), Some(text_run)) = (
            element.start_index,
            element.end_index,
            element.text_run.as_ref(),
        ) else {
            continue;
        };
        let overlap_start = element_start.max(source.start_index);
        let overlap_end = element_end.min(source.end_index);
        if overlap_start >= overlap_end {
            continue;
        }

        let content = utf16_slice(
            &text_run.content,
            overlap_start - element_start,
            overlap_end - element_start,
        );
        let range_start = docs_text_len(&source_text);
        source_text.push_str(&content);
        let range_end = docs_text_len(&source_text);
        if let Some(foreground_color) = text_run.text_style.foreground_color.clone()
            && range_end > range_start
        {
            push_merged_foreground_color_range(
                &mut ranges,
                DocsForegroundColorRange {
                    start: range_start,
                    end: range_end,
                    foreground_color,
                },
            );
        }
    }
    (source_text, ranges)
}

fn source_text_paragraph_alignment_ranges(
    source: DocsTextStyleSource<'_>,
) -> (String, Vec<DocsParagraphAlignmentRange>) {
    let mut source_text = String::new();
    let mut ranges = Vec::new();
    for (paragraph, range_start, range_end) in
        source_paragraph_text_ranges(source, &mut source_text)
    {
        if range_end <= range_start {
            continue;
        }
        let Some(alignment) = paragraph
            .paragraph_style
            .as_ref()
            .and_then(|style| style.alignment.clone())
            .filter(|alignment| alignment != "START")
        else {
            continue;
        };
        ranges.push(DocsParagraphAlignmentRange {
            start: range_start,
            end: range_end,
            alignment,
        });
    }
    (source_text, ranges)
}

fn source_text_paragraph_indent_ranges(
    source: DocsTextStyleSource<'_>,
) -> (String, Vec<DocsParagraphIndentRange>) {
    let mut source_text = String::new();
    let mut ranges = Vec::new();
    for (paragraph, range_start, range_end) in
        source_paragraph_text_ranges(source, &mut source_text)
    {
        if range_end <= range_start {
            continue;
        }
        let Some(style) = paragraph.paragraph_style.as_ref() else {
            continue;
        };
        if style.indent_start.is_none() && style.indent_first_line.is_none() {
            continue;
        }
        ranges.push(DocsParagraphIndentRange {
            start: range_start,
            end: range_end,
            indent_start: style.indent_start.clone(),
            indent_first_line: style.indent_first_line.clone(),
        });
    }
    (source_text, ranges)
}

fn source_paragraph_text_ranges<'a>(
    source: DocsTextStyleSource<'a>,
    source_text: &mut String,
) -> Vec<(&'a crate::docs_dto::Paragraph, usize, usize)> {
    let mut ranges = Vec::new();
    for paragraph in source
        .document
        .body
        .content
        .iter()
        .filter_map(|element| element.paragraph.as_ref())
    {
        let range_start = docs_text_len(source_text);
        for element in &paragraph.elements {
            let (Some(element_start), Some(element_end), Some(text_run)) = (
                element.start_index,
                element.end_index,
                element.text_run.as_ref(),
            ) else {
                continue;
            };
            let overlap_start = element_start.max(source.start_index);
            let overlap_end = element_end.min(source.end_index);
            if overlap_start >= overlap_end {
                continue;
            }
            source_text.push_str(&utf16_slice(
                &text_run.content,
                overlap_start - element_start,
                overlap_end - element_start,
            ));
        }
        let range_end = docs_text_len(source_text);
        ranges.push((paragraph, range_start, range_end));
    }
    ranges
}

fn source_text_background_ranges(
    source: DocsTextStyleSource<'_>,
) -> (String, Vec<DocsBackgroundColorRange>) {
    let mut source_text = String::new();
    let mut ranges = Vec::new();
    for element in source
        .document
        .body
        .content
        .iter()
        .filter_map(|element| element.paragraph.as_ref())
        .flat_map(|paragraph| paragraph.elements.iter())
    {
        let (Some(element_start), Some(element_end), Some(text_run)) = (
            element.start_index,
            element.end_index,
            element.text_run.as_ref(),
        ) else {
            continue;
        };
        let overlap_start = element_start.max(source.start_index);
        let overlap_end = element_end.min(source.end_index);
        if overlap_start >= overlap_end {
            continue;
        }

        let content = utf16_slice(
            &text_run.content,
            overlap_start - element_start,
            overlap_end - element_start,
        );
        let range_start = docs_text_len(&source_text);
        source_text.push_str(&content);
        let range_end = docs_text_len(&source_text);
        if let Some(background_color) = text_run.text_style.background_color.clone()
            && range_end > range_start
        {
            push_merged_background_color_range(
                &mut ranges,
                DocsBackgroundColorRange {
                    start: range_start,
                    end: range_end,
                    background_color,
                },
            );
        }
    }
    (source_text, ranges)
}

fn source_text_baseline_ranges(
    source: DocsTextStyleSource<'_>,
) -> (String, Vec<DocsBaselineOffsetRange>) {
    let mut source_text = String::new();
    let mut ranges = Vec::new();
    for element in source
        .document
        .body
        .content
        .iter()
        .filter_map(|element| element.paragraph.as_ref())
        .flat_map(|paragraph| paragraph.elements.iter())
    {
        let (Some(element_start), Some(element_end), Some(text_run)) = (
            element.start_index,
            element.end_index,
            element.text_run.as_ref(),
        ) else {
            continue;
        };
        let overlap_start = element_start.max(source.start_index);
        let overlap_end = element_end.min(source.end_index);
        if overlap_start >= overlap_end {
            continue;
        }

        let content = utf16_slice(
            &text_run.content,
            overlap_start - element_start,
            overlap_end - element_start,
        );
        let range_start = docs_text_len(&source_text);
        source_text.push_str(&content);
        let range_end = docs_text_len(&source_text);
        if let Some(baseline_offset) = text_run.text_style.baseline_offset.clone()
            && baseline_offset != "NONE"
            && range_end > range_start
        {
            push_merged_baseline_offset_range(
                &mut ranges,
                DocsBaselineOffsetRange {
                    start: range_start,
                    end: range_end,
                    baseline_offset,
                },
            );
        }
    }
    (source_text, ranges)
}

fn source_text_font_size_ranges(
    source: DocsTextStyleSource<'_>,
) -> (String, Vec<DocsFontSizeRange>) {
    let mut source_text = String::new();
    let mut ranges = Vec::new();
    for element in source
        .document
        .body
        .content
        .iter()
        .filter_map(|element| element.paragraph.as_ref())
        .flat_map(|paragraph| paragraph.elements.iter())
    {
        let (Some(element_start), Some(element_end), Some(text_run)) = (
            element.start_index,
            element.end_index,
            element.text_run.as_ref(),
        ) else {
            continue;
        };
        let overlap_start = element_start.max(source.start_index);
        let overlap_end = element_end.min(source.end_index);
        if overlap_start >= overlap_end {
            continue;
        }

        let content = utf16_slice(
            &text_run.content,
            overlap_start - element_start,
            overlap_end - element_start,
        );
        let range_start = docs_text_len(&source_text);
        source_text.push_str(&content);
        let range_end = docs_text_len(&source_text);
        if let Some(font_size) = text_run.text_style.font_size.clone()
            && range_end > range_start
        {
            push_merged_font_size_range(
                &mut ranges,
                DocsFontSizeRange {
                    start: range_start,
                    end: range_end,
                    font_size,
                },
            );
        }
    }
    (source_text, ranges)
}

fn source_text_font_family_ranges(
    source: DocsTextStyleSource<'_>,
) -> (String, Vec<DocsFontFamilyRange>) {
    let mut source_text = String::new();
    let mut ranges = Vec::new();
    for element in source
        .document
        .body
        .content
        .iter()
        .filter_map(|element| element.paragraph.as_ref())
        .flat_map(|paragraph| paragraph.elements.iter())
    {
        let (Some(element_start), Some(element_end), Some(text_run)) = (
            element.start_index,
            element.end_index,
            element.text_run.as_ref(),
        ) else {
            continue;
        };
        let overlap_start = element_start.max(source.start_index);
        let overlap_end = element_end.min(source.end_index);
        if overlap_start >= overlap_end {
            continue;
        }

        let content = utf16_slice(
            &text_run.content,
            overlap_start - element_start,
            overlap_end - element_start,
        );
        let range_start = docs_text_len(&source_text);
        source_text.push_str(&content);
        let range_end = docs_text_len(&source_text);
        if let Some(weighted_font_family) = text_run.text_style.weighted_font_family.clone()
            && range_end > range_start
        {
            push_merged_font_family_range(
                &mut ranges,
                DocsFontFamilyRange {
                    start: range_start,
                    end: range_end,
                    weighted_font_family,
                },
            );
        }
    }
    (source_text, ranges)
}

fn source_text_small_caps_ranges(
    source: DocsTextStyleSource<'_>,
) -> (String, Vec<DocsSmallCapsRange>) {
    let mut source_text = String::new();
    let mut ranges = Vec::new();
    for element in source
        .document
        .body
        .content
        .iter()
        .filter_map(|element| element.paragraph.as_ref())
        .flat_map(|paragraph| paragraph.elements.iter())
    {
        let (Some(element_start), Some(element_end), Some(text_run)) = (
            element.start_index,
            element.end_index,
            element.text_run.as_ref(),
        ) else {
            continue;
        };
        let overlap_start = element_start.max(source.start_index);
        let overlap_end = element_end.min(source.end_index);
        if overlap_start >= overlap_end {
            continue;
        }

        let content = utf16_slice(
            &text_run.content,
            overlap_start - element_start,
            overlap_end - element_start,
        );
        let range_start = docs_text_len(&source_text);
        source_text.push_str(&content);
        let range_end = docs_text_len(&source_text);
        if text_run.text_style.small_caps && range_end > range_start {
            push_merged_small_caps_range(
                &mut ranges,
                DocsSmallCapsRange {
                    start: range_start,
                    end: range_end,
                },
            );
        }
    }
    (source_text, ranges)
}

fn push_merged_foreground_color_range(
    ranges: &mut Vec<DocsForegroundColorRange>,
    range: DocsForegroundColorRange,
) {
    if let Some(previous) = ranges.last_mut()
        && previous.end == range.start
        && previous.foreground_color == range.foreground_color
    {
        previous.end = range.end;
        return;
    }
    ranges.push(range);
}

fn push_merged_background_color_range(
    ranges: &mut Vec<DocsBackgroundColorRange>,
    range: DocsBackgroundColorRange,
) {
    if let Some(previous) = ranges.last_mut()
        && previous.end == range.start
        && previous.background_color == range.background_color
    {
        previous.end = range.end;
        return;
    }
    ranges.push(range);
}

fn push_merged_baseline_offset_range(
    ranges: &mut Vec<DocsBaselineOffsetRange>,
    range: DocsBaselineOffsetRange,
) {
    if let Some(previous) = ranges.last_mut()
        && previous.end == range.start
        && previous.baseline_offset == range.baseline_offset
    {
        previous.end = range.end;
        return;
    }
    ranges.push(range);
}

fn push_merged_font_size_range(ranges: &mut Vec<DocsFontSizeRange>, range: DocsFontSizeRange) {
    if let Some(previous) = ranges.last_mut()
        && previous.end == range.start
        && previous.font_size == range.font_size
    {
        previous.end = range.end;
        return;
    }
    ranges.push(range);
}

fn push_merged_font_family_range(
    ranges: &mut Vec<DocsFontFamilyRange>,
    range: DocsFontFamilyRange,
) {
    if let Some(previous) = ranges.last_mut()
        && previous.end == range.start
        && previous.weighted_font_family == range.weighted_font_family
    {
        previous.end = range.end;
        return;
    }
    ranges.push(range);
}

fn push_merged_small_caps_range(ranges: &mut Vec<DocsSmallCapsRange>, range: DocsSmallCapsRange) {
    if let Some(previous) = ranges.last_mut()
        && previous.end == range.start
    {
        previous.end = range.end;
        return;
    }
    ranges.push(range);
}

fn map_source_range_by_context(
    source_text: &str,
    source_start: usize,
    source_end: usize,
    new_text: &str,
) -> Option<(usize, usize)> {
    let source_len = docs_text_len(source_text);
    let new_len = docs_text_len(new_text);
    if source_start > source_end || source_end > source_len {
        return None;
    }
    if source_len == new_len {
        return (source_end > source_start).then_some((source_start, source_end));
    }

    let common_prefix = common_prefix_utf16(source_text, new_text);
    let common_suffix = common_suffix_utf16(
        source_text,
        new_text,
        source_len.saturating_sub(common_prefix),
        new_len.saturating_sub(common_prefix),
    );
    let old_change_start = common_prefix;
    let old_change_end = source_len.saturating_sub(common_suffix);
    let new_change_start = common_prefix;
    let new_change_end = new_len.saturating_sub(common_suffix);

    let (start, end) = if source_end <= old_change_start {
        let mut end = source_end;
        if source_end == old_change_start
            && old_change_start == old_change_end
            && should_extend_color_boundary_insertion(new_text, new_change_start, new_change_end)
        {
            end = new_change_end;
        }
        (source_start, end)
    } else if source_start >= old_change_end {
        (
            shift_utf16_index(source_start, old_change_end, new_change_end)?,
            shift_utf16_index(source_end, old_change_end, new_change_end)?,
        )
    } else {
        let start = if source_start < old_change_start {
            source_start
        } else {
            new_change_start
        };
        let end = if source_end > old_change_end {
            shift_utf16_index(source_end, old_change_end, new_change_end)?
        } else {
            new_change_end
        };
        (start, end)
    };
    (end > start).then_some((start, end))
}

fn map_paragraph_range_by_context(
    source_text: &str,
    source_start: usize,
    source_end: usize,
    new_text: &str,
) -> Option<(usize, usize)> {
    let source_len = docs_text_len(source_text);
    let new_len = docs_text_len(new_text);
    if source_start > source_end || source_end > source_len {
        return None;
    }
    if source_len == new_len {
        return (source_end > source_start).then_some((source_start, source_end));
    }

    let common_prefix = common_prefix_utf16(source_text, new_text);
    let common_suffix = common_suffix_utf16(
        source_text,
        new_text,
        source_len.saturating_sub(common_prefix),
        new_len.saturating_sub(common_prefix),
    );
    let old_change_start = common_prefix;
    let old_change_end = source_len.saturating_sub(common_suffix);
    let new_change_start = common_prefix;
    let new_change_end = new_len.saturating_sub(common_suffix);

    let (start, end) = if source_end <= old_change_start {
        let mut end = source_end;
        if source_end == old_change_start && old_change_start == old_change_end {
            end = new_change_end;
        }
        (source_start, end)
    } else if source_start >= old_change_end {
        (
            shift_utf16_index(source_start, old_change_end, new_change_end)?,
            shift_utf16_index(source_end, old_change_end, new_change_end)?,
        )
    } else {
        let start = if source_start < old_change_start {
            source_start
        } else {
            new_change_start
        };
        let end = if source_end > old_change_end {
            shift_utf16_index(source_end, old_change_end, new_change_end)?
        } else {
            new_change_end
        };
        (start, end)
    };
    (end > start).then_some((start, end))
}

fn common_prefix_utf16(left: &str, right: &str) -> usize {
    let mut units = 0;
    for (left_ch, right_ch) in left.chars().zip(right.chars()) {
        if left_ch != right_ch {
            break;
        }
        units += left_ch.len_utf16();
    }
    units
}

fn common_suffix_utf16(
    left: &str,
    right: &str,
    max_left_units: usize,
    max_right_units: usize,
) -> usize {
    let mut units = 0;
    for (left_ch, right_ch) in left.chars().rev().zip(right.chars().rev()) {
        if left_ch != right_ch {
            break;
        }
        let ch_units = left_ch.len_utf16();
        if units + ch_units > max_left_units || units + ch_units > max_right_units {
            break;
        }
        units += ch_units;
    }
    units
}

fn shift_utf16_index(index: usize, old_change_end: usize, new_change_end: usize) -> Option<usize> {
    if new_change_end >= old_change_end {
        index.checked_add(new_change_end - old_change_end)
    } else {
        index.checked_sub(old_change_end - new_change_end)
    }
}

fn should_extend_color_boundary_insertion(
    new_text: &str,
    insertion_start: usize,
    insertion_end: usize,
) -> bool {
    if insertion_start >= insertion_end {
        return false;
    }
    utf16_slice(new_text, insertion_start, insertion_end)
        .chars()
        .next()
        .is_some_and(|ch| !ch.is_whitespace())
}

fn ranges_overlap(
    left_start: usize,
    left_end: usize,
    right_start: usize,
    right_end: usize,
) -> bool {
    left_start < right_end && right_start < left_end
}

fn utf16_slice(value: &str, start: usize, end: usize) -> String {
    let mut sliced = String::new();
    let mut offset = 0;
    for ch in value.chars() {
        let next = offset + ch.len_utf16();
        if offset >= end {
            break;
        }
        if offset >= start && next <= end {
            sliced.push(ch);
        }
        offset = next;
    }
    sliced
}

fn text_style_at(document: &GoogleDocument, index: usize) -> Option<&TextStyle> {
    document
        .body
        .content
        .iter()
        .filter_map(|element| element.paragraph.as_ref())
        .flat_map(|paragraph| paragraph.elements.iter())
        .find(|element| {
            let Some(start_index) = element.start_index else {
                return false;
            };
            let Some(end_index) = element.end_index else {
                return false;
            };
            start_index <= index && index < end_index && element.text_run.is_some()
        })
        .and_then(|element| element.text_run.as_ref())
        .map(|text_run| &text_run.text_style)
}

fn docs_text(content: &str) -> DocsText {
    let mut parsed = parse_docs_markdown_inline(content);
    if !parsed.text.ends_with('\n') {
        parsed.text.push('\n');
    }
    let final_newline_start = parsed.text.len().saturating_sub('\n'.len_utf8());
    parsed.text = parsed
        .text
        .char_indices()
        .map(|(index, ch)| {
            if ch == '\n' && index < final_newline_start {
                '\u{000b}'
            } else {
                ch
            }
        })
        .collect();
    parsed
}

fn docs_document_text(content: &str) -> DocsText {
    let mut parsed = parse_docs_markdown_blocks(content);
    if !parsed.text.ends_with('\n') {
        parsed.text.push('\n');
    }
    parsed
}

fn docs_block_text(content: &str) -> DocsText {
    let trimmed = content.trim_start();
    let (block_content, block_kind) = markdown_block_content(trimmed);
    if matches!(block_kind, MarkdownBlockKind::Paragraph) {
        let mut parsed = if block_content == trimmed {
            docs_text(content)
        } else {
            docs_text(block_content)
        };
        let end = docs_text_len(&parsed.text);
        if end > 0 {
            parsed.paragraph_styles.push(DocsParagraphStyleRange {
                start: 0,
                end,
                named_style_type: "NORMAL_TEXT".to_string(),
            });
        }
        return parsed;
    }

    let mut parsed = DocsText::default();
    append_markdown_block(&mut parsed, content);
    if matches!(
        block_kind,
        MarkdownBlockKind::UnorderedList | MarkdownBlockKind::OrderedList
    ) {
        parsed.list_block = true;
    }
    if !parsed.text.ends_with('\n') {
        parsed.text.push('\n');
    }
    parsed
}

fn strip_trailing_segment_newline(docs_text: &mut DocsText) {
    if !docs_text.text.ends_with('\n') {
        return;
    }
    let old_len = docs_text_len(&docs_text.text);
    docs_text.text.pop();
    let new_len = docs_text_len(&docs_text.text);
    for range in &mut docs_text.style_ranges {
        if range.end == old_len {
            range.end = new_len;
        }
    }
    docs_text
        .style_ranges
        .retain(|range| range.end > range.start);
    for range in &mut docs_text.paragraph_styles {
        if range.end == old_len {
            range.end = new_len;
        }
    }
    docs_text
        .paragraph_styles
        .retain(|range| range.end > range.start);
    for range in &mut docs_text.bullet_ranges {
        if range.end == old_len {
            range.end = new_len;
        }
    }
    docs_text
        .bullet_ranges
        .retain(|range| range.end > range.start);
}

fn parse_docs_markdown_blocks(content: &str) -> DocsText {
    let mut parsed = DocsText::default();
    let mut current = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            append_markdown_block(&mut parsed, &current.join("\n"));
            current.clear();
        } else {
            current.push(line);
        }
    }
    append_markdown_block(&mut parsed, &current.join("\n"));
    parsed
}

fn append_markdown_block(parsed: &mut DocsText, block: &str) {
    if block.trim().is_empty() {
        return;
    }
    let block_start = docs_text_len(&parsed.text);
    let trimmed = block.trim_start();
    let (content, block_kind) = markdown_block_content(trimmed);
    let block_inline = parse_docs_markdown_inline(content);
    if matches!(
        block_kind,
        MarkdownBlockKind::UnorderedList | MarkdownBlockKind::OrderedList
    ) {
        parsed
            .text
            .push_str(&"\t".repeat(markdown_list_nesting_level(block)));
    }
    append_parsed_inline(parsed, &block_inline);
    if !parsed.text.ends_with('\n') {
        parsed.text.push('\n');
    }
    let block_end = docs_text_len(&parsed.text);
    match block_kind {
        MarkdownBlockKind::Heading(level) => {
            parsed.paragraph_styles.push(DocsParagraphStyleRange {
                start: block_start,
                end: block_end,
                named_style_type: format!("HEADING_{level}"),
            })
        }
        MarkdownBlockKind::UnorderedList => parsed.bullet_ranges.push(DocsBulletRange {
            start: block_start,
            end: block_end,
            preset: "BULLET_DISC_CIRCLE_SQUARE".to_string(),
        }),
        MarkdownBlockKind::OrderedList => parsed.bullet_ranges.push(DocsBulletRange {
            start: block_start,
            end: block_end,
            preset: "NUMBERED_DECIMAL_ALPHA_ROMAN".to_string(),
        }),
        MarkdownBlockKind::Paragraph => {}
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkdownBlockKind {
    Paragraph,
    Heading(usize),
    UnorderedList,
    OrderedList,
}

fn markdown_block_content(block: &str) -> (&str, MarkdownBlockKind) {
    if let Some(content) = escaped_markdown_block_content(block) {
        return (content, MarkdownBlockKind::Paragraph);
    }
    if let Some((level, content)) = markdown_heading_content(block) {
        return (content, MarkdownBlockKind::Heading(level));
    }
    if block.starts_with("- ") || block.starts_with("* ") || block.starts_with("+ ") {
        return (&block[2..], MarkdownBlockKind::UnorderedList);
    }
    if let Some((_, content)) = markdown_ordered_list_content(block) {
        return (content, MarkdownBlockKind::OrderedList);
    }
    (block, MarkdownBlockKind::Paragraph)
}

fn escaped_markdown_block_content(block: &str) -> Option<&str> {
    let content = block.strip_prefix('\\')?;
    paragraph_block_start_marker_needs_escape(content).then_some(content)
}

fn paragraph_block_start_marker_needs_escape(value: &str) -> bool {
    value.starts_with("::loc")
        || markdown_heading_content(value).is_some()
        || value.starts_with("- ")
        || value.starts_with("* ")
        || value.starts_with("+ ")
        || markdown_ordered_list_content(value).is_some()
        || value.starts_with("> ")
        || value.trim_end() == "---"
}

fn markdown_heading_content(block: &str) -> Option<(usize, &str)> {
    let level = block.chars().take_while(|ch| *ch == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &block[level..];
    rest.strip_prefix(' ').map(|content| (level, content))
}

fn markdown_ordered_list_content(block: &str) -> Option<(&str, &str)> {
    let (digits, content) = block.split_once(". ")?;
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some((digits, content))
}

fn markdown_list_nesting_level(block: &str) -> usize {
    let leading = block
        .char_indices()
        .find(|(_, ch)| !matches!(ch, ' ' | '\t'))
        .map(|(index, _)| &block[..index])
        .unwrap_or(block);
    let mut nesting = 0;
    let mut spaces = 0;
    for ch in leading.chars() {
        match ch {
            '\t' => {
                nesting += 1;
                spaces = 0;
            }
            ' ' => {
                spaces += 1;
                if spaces == 2 {
                    nesting += 1;
                    spaces = 0;
                }
            }
            _ => {}
        }
    }
    nesting
}

fn parse_docs_markdown_inline(content: &str) -> DocsText {
    let mut parsed = DocsText {
        list_block: starts_with_markdown_list_marker(content),
        ..DocsText::default()
    };
    let mut index = 0;
    while index < content.len() {
        if let Some(marker) = escaped_literal_inline_marker_prefix(&content[index..]) {
            parsed.text.push_str(marker);
            index += '\\'.len_utf8() + marker.len();
            continue;
        }
        if content[index..].starts_with("\\\\") {
            parsed.text.push('\\');
            index += 2;
            continue;
        }
        if let Some(next) = parse_markdown_span(content, index, &mut parsed) {
            index = next;
            continue;
        }

        let ch = content[index..]
            .chars()
            .next()
            .expect("index is inside content");
        parsed.text.push(ch);
        index += ch.len_utf8();
    }
    parsed
}

fn starts_with_markdown_list_marker(content: &str) -> bool {
    let trimmed = content.trim_start();
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return true;
    }
    let Some((digits, _rest)) = trimmed.split_once(". ") else {
        return false;
    };
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

fn parse_markdown_span(content: &str, index: usize, parsed: &mut DocsText) -> Option<usize> {
    if content[index..].starts_with("**") {
        return parse_delimited_style(
            content,
            index,
            "**",
            "**",
            DocsInlineStyle {
                bold: true,
                ..DocsInlineStyle::default()
            },
            parsed,
        );
    }
    if content[index..].starts_with("~~") {
        return parse_delimited_style(
            content,
            index,
            "~~",
            "~~",
            DocsInlineStyle {
                strikethrough: true,
                ..DocsInlineStyle::default()
            },
            parsed,
        );
    }
    if content[index..].starts_with("<u>") {
        return parse_delimited_style(
            content,
            index,
            "<u>",
            "</u>",
            DocsInlineStyle {
                underline: true,
                ..DocsInlineStyle::default()
            },
            parsed,
        );
    }
    if content[index..].starts_with('[') {
        return parse_link_style(content, index, parsed);
    }
    if content[index..].starts_with('*') && !content[index..].starts_with("**") {
        return parse_delimited_style(
            content,
            index,
            "*",
            "*",
            DocsInlineStyle {
                italic: true,
                ..DocsInlineStyle::default()
            },
            parsed,
        );
    }
    None
}

fn escaped_literal_inline_marker_prefix(value: &str) -> Option<&'static str> {
    literal_inline_tag_prefix(value).or_else(|| {
        ["**", "~~", "`", "[", "]", "_"].into_iter().find(|marker| {
            value
                .strip_prefix('\\')
                .is_some_and(|rest| rest.starts_with(marker))
        })
    })
}

fn literal_inline_tag_prefix(value: &str) -> Option<&'static str> {
    ["<br />", "<br/>", "<br>", "</u>", "<u>"]
        .into_iter()
        .find(|tag| {
            value
                .strip_prefix('\\')
                .is_some_and(|rest| rest.starts_with(tag))
        })
}

fn parse_delimited_style(
    content: &str,
    index: usize,
    open: &str,
    close: &str,
    style: DocsInlineStyle,
    parsed: &mut DocsText,
) -> Option<usize> {
    let inner_start = index + open.len();
    let close_offset = content[inner_start..].find(close)?;
    let inner_end = inner_start + close_offset;
    let start = docs_text_len(&parsed.text);
    append_parsed_inline(
        parsed,
        &parse_docs_markdown_inline(&content[inner_start..inner_end]),
    );
    let end = docs_text_len(&parsed.text);
    push_style_range(parsed, start, end, style);
    Some(inner_end + close.len())
}

fn parse_link_style(content: &str, index: usize, parsed: &mut DocsText) -> Option<usize> {
    let label_start = index + '['.len_utf8();
    let label_end = find_unescaped_link_label_end(content, label_start)?;
    let url_start = label_end + "](".len();
    let url_end = find_unescaped_char(content, url_start, ')')?;
    let start = docs_text_len(&parsed.text);
    append_parsed_inline(
        parsed,
        &parse_docs_markdown_inline(&content[label_start..label_end]),
    );
    let end = docs_text_len(&parsed.text);
    push_style_range(
        parsed,
        start,
        end,
        DocsInlineStyle {
            link: Some(unescape_markdown_link_href(&content[url_start..url_end])),
            ..DocsInlineStyle::default()
        },
    );
    Some(url_end + ')'.len_utf8())
}

fn find_unescaped_link_label_end(content: &str, start: usize) -> Option<usize> {
    let mut index = start;
    while index < content.len() {
        let ch = content[index..].chars().next()?;
        if ch == '\\' {
            index += ch.len_utf8();
            if let Some(escaped) = content[index..].chars().next() {
                index += escaped.len_utf8();
            }
            continue;
        }
        if ch == ']' && content[index + ch.len_utf8()..].starts_with('(') {
            return Some(index);
        }
        index += ch.len_utf8();
    }
    None
}

fn find_unescaped_char(content: &str, start: usize, needle: char) -> Option<usize> {
    let mut index = start;
    while index < content.len() {
        let ch = content[index..].chars().next()?;
        if ch == '\\' {
            index += ch.len_utf8();
            if let Some(escaped) = content[index..].chars().next() {
                index += escaped.len_utf8();
            }
            continue;
        }
        if ch == needle {
            return Some(index);
        }
        index += ch.len_utf8();
    }
    None
}

fn unescape_markdown_link_href(href: &str) -> String {
    let mut unescaped = String::with_capacity(href.len());
    let mut index = 0;
    while index < href.len() {
        let ch = href[index..].chars().next().expect("index inside href");
        if ch == '\\' {
            index += ch.len_utf8();
            if let Some(escaped) = href[index..].chars().next() {
                if matches!(escaped, '\\' | '(' | ')') {
                    unescaped.push(escaped);
                    index += escaped.len_utf8();
                    continue;
                }
                unescaped.push(ch);
                unescaped.push(escaped);
                index += escaped.len_utf8();
                continue;
            }
            unescaped.push(ch);
            continue;
        }
        unescaped.push(ch);
        index += ch.len_utf8();
    }
    unescaped
}

fn append_parsed_inline(parsed: &mut DocsText, inline: &DocsText) {
    let offset = docs_text_len(&parsed.text);
    parsed.text.push_str(&inline.text);
    parsed
        .style_ranges
        .extend(inline.style_ranges.iter().cloned().map(|mut range| {
            range.start += offset;
            range.end += offset;
            range
        }));
}

fn push_style_range(parsed: &mut DocsText, start: usize, end: usize, style: DocsInlineStyle) {
    if end > start {
        parsed
            .style_ranges
            .push(DocsTextStyleRange { start, end, style });
    }
}

fn docs_text_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn document_end_index(document: &GoogleDocument) -> usize {
    document
        .body
        .content
        .iter()
        .filter_map(|element| element.end_index)
        .max()
        .unwrap_or(1)
}

fn document_start_index(document: &GoogleDocument) -> usize {
    document
        .body
        .content
        .iter()
        .filter_map(|element| element.start_index)
        .filter(|index| *index > 0)
        .min()
        .unwrap_or(1)
}

fn empty_document(id: &str, title: &str) -> GoogleDocument {
    GoogleDocument {
        document_id: id.to_string(),
        title: title.to_string(),
        ..GoogleDocument::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GoogleBlockRange {
    document_id: String,
    start_index: usize,
    end_index: usize,
}

impl GoogleBlockRange {
    fn parse(remote_id: &RemoteId) -> LocalityResult<Self> {
        let mut parts = remote_id.0.rsplitn(3, ':').collect::<Vec<_>>();
        if parts.len() != 3 {
            return Err(LocalityError::InvalidState(format!(
                "google docs block id `{}` is not a range id",
                remote_id.0
            )));
        }
        parts.reverse();
        let start_index = parts[1].parse::<usize>().map_err(|_| {
            LocalityError::InvalidState(format!(
                "google docs block id `{}` has invalid start",
                remote_id.0
            ))
        })?;
        let end_index = parts[2].parse::<usize>().map_err(|_| {
            LocalityError::InvalidState(format!(
                "google docs block id `{}` has invalid end",
                remote_id.0
            ))
        })?;
        Ok(Self {
            document_id: parts[0].to_string(),
            start_index,
            end_index,
        })
    }

    fn shifted_for_insertions(&self, insertions: &[(usize, usize)]) -> Self {
        Self {
            document_id: self.document_id.clone(),
            start_index: shift_index_for_insertions(self.start_index, insertions),
            end_index: shift_index_for_insertions(self.end_index, insertions),
        }
    }
}

fn shift_index_for_insertions(index: usize, insertions: &[(usize, usize)]) -> usize {
    insertions.iter().fold(index, |shifted, (insert_at, len)| {
        if *insert_at <= shifted {
            shifted + len
        } else {
            shifted
        }
    })
}

fn allocate_path(
    parent_path: &Path,
    title: &str,
    remote_id: &str,
    page: bool,
    used_paths: &mut BTreeSet<PathBuf>,
) -> PathBuf {
    let base = slugify_title(title);
    for suffix in [
        None,
        Some(short_id(remote_id, 6)),
        Some(short_id(remote_id, 8)),
    ] {
        let stem = suffix
            .as_ref()
            .map(|suffix| format!("{base} {suffix}"))
            .unwrap_or_else(|| base.clone());
        let path = if page {
            page_document_path(&parent_path.join(&stem))
        } else {
            parent_path.join(&stem)
        };
        let mut reservations = vec![path.clone()];
        if page {
            reservations.push(page_container_path(&path));
            reservations.push(parent_path.join(format!("{stem}.md")));
        }
        if reservations.iter().all(|path| !used_paths.contains(path)) {
            used_paths.extend(reservations);
            return path;
        }
    }
    let stem = format!("{base} {}", short_id(remote_id, 32));
    if page {
        page_document_path(&parent_path.join(stem))
    } else {
        parent_path.join(stem)
    }
}

fn slugify_title(title: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "untitled".to_string()
    } else {
        slug
    }
}

fn short_id(remote_id: &str, len: usize) -> String {
    let short = remote_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(len)
        .collect::<String>();
    if short.is_empty() {
        "id".to_string()
    } else {
        short
    }
}

pub fn extract_google_drive_folder_id(value: &str) -> Option<String> {
    if let Some(after_folders) = value.split("/folders/").nth(1) {
        return Some(
            after_folders
                .split(['?', '/', '#'])
                .next()
                .unwrap_or(after_folders)
                .to_string(),
        );
    }
    if let Some(after_id) = value.split("id=").nth(1) {
        return Some(
            after_id
                .split(['&', '#'])
                .next()
                .unwrap_or(after_id)
                .to_string(),
        );
    }
    None
}

fn looks_like_google_drive_id(value: &str) -> bool {
    value.len() >= 10
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use locality_connector::{
        ApplyPlanRequest, Connector, EnumerateRequest, FetchRequest, ObserveRequest,
    };
    use locality_core::journal::{PushId, PushOperationId};
    use locality_core::model::{EntityKind, MountId, RemoteId};
    use locality_core::planner::{PushOperation, PushPlan};
    use locality_core::push::RemotePrecondition;

    use super::{
        GoogleDocsConfig, GoogleDocsConnector, docs_block_text, docs_document_text_requests,
        docs_text_len,
    };
    use crate::client::{GoogleDocsApi, GoogleDriveApi};
    use crate::docs_dto::{BatchUpdateDocumentRequest, DocsRequest, GoogleDocument, Range};
    use crate::drive_dto::{
        DriveCreateFileRequest, DriveFile, DriveFileList, DriveUpdateFileRequest,
    };

    #[test]
    fn enumerate_projects_workspace_folders_and_docs_as_page_directories() {
        let drive = Arc::new(
            FakeDrive::default()
                .with_children(
                    "workspace",
                    vec![folder("folder-1", "Marketing", "workspace")],
                )
                .with_children(
                    "folder-1",
                    vec![
                        doc_file("doc-1", "Launch Brief", "folder-1"),
                        doc_file("doc-2", "Nested Doc", "folder-1"),
                    ],
                ),
        );
        let connector = GoogleDocsConnector::with_apis(
            GoogleDocsConfig::new("token").with_workspace_folder_id(RemoteId::new("workspace")),
            drive,
            Arc::new(FakeDocs::default()),
        );

        let entries = connector
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("google-docs-main"),
                cursor: None,
            })
            .expect("enumerate");

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].kind, EntityKind::Directory);
        assert_eq!(entries[0].path, std::path::Path::new("marketing"));
        assert_eq!(entries[1].kind, EntityKind::Page);
        assert_eq!(
            entries[1].path,
            std::path::Path::new("marketing/launch-brief/page.md")
        );
        assert_eq!(
            entries[1]
                .stub_frontmatter
                .as_ref()
                .unwrap()
                .contains("google-docs"),
            true
        );
        assert_eq!(
            entries[2].path,
            std::path::Path::new("marketing/nested-doc/page.md")
        );
    }

    #[test]
    fn fetch_gets_drive_metadata_and_document_body() {
        let drive = Arc::new(FakeDrive::default().with_file(doc_file(
            "doc-1",
            "Launch Brief",
            "workspace",
        )));
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "doc-1",
            "Launch Brief",
            "rev-1",
            "Hello\n",
        )));
        let connector = GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs);

        let native = connector
            .fetch(FetchRequest {
                remote_id: RemoteId::new("doc-1"),
            })
            .expect("fetch");

        assert_eq!(native.kind, "google_docs_document");
        assert!(
            String::from_utf8(native.raw)
                .unwrap()
                .contains("Launch Brief")
        );
    }

    #[test]
    fn observe_reports_remote_version_from_drive_and_docs() {
        let drive = Arc::new(FakeDrive::default().with_file(doc_file(
            "doc-1",
            "Launch Brief",
            "workspace",
        )));
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "doc-1",
            "Launch Brief",
            "rev-1",
            "Hello\n",
        )));
        let connector = GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs);

        let observation = connector
            .observe(ObserveRequest {
                mount_id: MountId::new("google-docs-main"),
                remote_id: RemoteId::new("doc-1"),
            })
            .expect("observe");

        assert_eq!(
            observation.remote_version.unwrap().as_str(),
            "drive:7:2026-06-25T10:00:00.000Z|docs:rev-1"
        );
    }

    #[test]
    fn apply_uses_required_revision_for_body_update_and_trashes_deletes() {
        let drive = Arc::new(FakeDrive::default().with_file(doc_file(
            "doc-1",
            "Launch Brief",
            "workspace",
        )));
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "doc-1",
            "Launch Brief",
            "rev-1",
            "Hello\n",
        )));
        let connector = GoogleDocsConnector::with_apis(
            GoogleDocsConfig::new("token"),
            drive.clone(),
            docs.clone(),
        );
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![
                PushOperation::UpdateBlock {
                    block_id: RemoteId::new("doc-1:1:7"),
                    content: "Updated".to_string(),
                },
                PushOperation::ArchiveEntity {
                    entity_id: RemoteId::new("doc-1"),
                },
            ],
        );
        let op_ids = vec![
            PushOperationId("push-1:0:update_block:doc-1".to_string()),
            PushOperationId("push-1:1:archive_entity:doc-1".to_string()),
        ];
        let preconditions = vec![RemotePrecondition {
            remote_id: RemoteId::new("doc-1"),
            remote_edited_at: Some("drive:7:2026-06-25T10:00:00.000Z|docs:rev-1".to_string()),
        }];

        connector
            .check_concurrency(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &preconditions,
                local_root: None,
            })
            .expect("concurrency");
        let result = connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &preconditions,
                local_root: None,
            })
            .expect("apply");

        assert_eq!(result.changed_remote_ids, vec![RemoteId::new("doc-1")]);
        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        assert_eq!(
            batch.write_control.unwrap().required_revision_id.as_deref(),
            Some("rev-1")
        );
        assert_eq!(
            drive
                .last_update
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .1
                .trashed,
            Some(true)
        );
    }

    #[test]
    fn concurrency_allows_drive_only_version_drift_when_docs_revision_matches() {
        let mut file = doc_file("doc-1", "Launch Brief", "workspace");
        file.version = Some("8".to_string());
        let drive = Arc::new(FakeDrive::default().with_file(file));
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "doc-1",
            "Launch Brief",
            "rev-1",
            "Hello\n",
        )));
        let connector = GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs);
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::AppendBlock {
                parent_id: RemoteId::new("doc-1"),
                after: Some(RemoteId::new("doc-1:1:7")),
                content: "Local body edit".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:append_block:doc-1".to_string())];
        let preconditions = vec![RemotePrecondition {
            remote_id: RemoteId::new("doc-1"),
            remote_edited_at: Some("drive:7:2026-06-25T10:00:00.000Z|docs:rev-1".to_string()),
        }];

        connector
            .check_concurrency(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &preconditions,
                local_root: None,
            })
            .expect("drive-only version drift should not block body push");
    }

    #[test]
    fn apply_converts_markdown_inline_styles_to_docs_text() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Pet Resume", "workspace")));
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "doc-1",
            "Pet Resume",
            "rev-1",
            "Age: 4 years\u{000b}Weight: 33 pounds\n",
        )));
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:36"),
                content: "**Age:** 4 years\n**Weight:** 34 pounds".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let DocsRequest::InsertText { insert_text } = &batch.requests[1] else {
            panic!("expected insert text request");
        };
        assert_eq!(insert_text.text, "Age: 4 years\u{000b}Weight: 34 pounds\n");
        let DocsRequest::UpdateTextStyle { update_text_style } = &batch.requests[2] else {
            panic!("expected style reset request");
        };
        assert_eq!(update_text_style.range.start_index, 1);
        assert_eq!(update_text_style.range.end_index, 32);
        assert_eq!(update_text_style.text_style.bold, Some(false));
        let DocsRequest::UpdateParagraphStyle {
            update_paragraph_style,
        } = &batch.requests[4]
        else {
            panic!("expected paragraph style request");
        };
        assert_eq!(update_paragraph_style.range.start_index, 1);
        assert_eq!(update_paragraph_style.range.end_index, 32);
        assert_eq!(
            update_paragraph_style
                .paragraph_style
                .named_style_type
                .as_deref(),
            Some("NORMAL_TEXT")
        );
        let DocsRequest::UpdateTextStyle { update_text_style } = &batch.requests[5] else {
            panic!("expected age style request");
        };
        assert_eq!(update_text_style.range.start_index, 1);
        assert_eq!(update_text_style.range.end_index, 5);
        assert_eq!(update_text_style.text_style.bold, Some(true));
        let DocsRequest::UpdateTextStyle { update_text_style } = &batch.requests[6] else {
            panic!("expected weight style request");
        };
        assert_eq!(update_text_style.range.start_index, 14);
        assert_eq!(update_text_style.range.end_index, 21);
        assert_eq!(update_text_style.text_style.bold, Some(true));
    }

    #[test]
    fn apply_decodes_escaped_literal_markdown_inline_markers() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Literal Doc", "workspace")));
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "doc-1",
            "Literal Doc",
            "rev-1",
            "Original\n",
        )));
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let literal = "Literal **bold** _italic_ ~~strike~~ `code` [link](https://example.com) <u>underline</u>";
        let escaped = "Literal \\**bold\\** \\_italic\\_ \\~~strike\\~~ \\`code\\` \\[link](https://example.com) \\<u>underline\\</u>";
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:10"),
                content: escaped.to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let DocsRequest::InsertText { insert_text } = &batch.requests[1] else {
            panic!("expected insert text request");
        };
        assert_eq!(insert_text.text, literal);
        assert_eq!(
            batch
                .requests
                .iter()
                .filter(|request| matches!(request, DocsRequest::UpdateTextStyle { .. }))
                .count(),
            1,
            "escaped literal markers should not emit inline style requests beyond the reset"
        );
    }

    #[test]
    fn apply_decodes_escaped_parentheses_in_markdown_link_hrefs() {
        let parsed = docs_block_text(
            "[<u>Reference v2</u>](https://example.test/path\\(abc\\)?q=one\\(two\\)) link",
        );

        assert_eq!(parsed.text, "Reference v2 link\n");
        assert!(
            parsed.style_ranges.iter().any(|range| {
                range.start == 0
                    && range.end == docs_text_len("Reference v2")
                    && range.style.link.as_deref()
                        == Some("https://example.test/path(abc)?q=one(two)")
            }),
            "expected escaped Markdown href parentheses to decode into one link style: {:#?}",
            parsed.style_ranges
        );
        assert!(
            parsed.style_ranges.iter().any(|range| {
                range.start == 0
                    && range.end == docs_text_len("Reference v2")
                    && range.style.underline
            }),
            "expected nested underline to remain scoped to the linked label: {:#?}",
            parsed.style_ranges
        );
    }

    #[test]
    fn apply_ignores_escaped_link_label_delimiters() {
        let parsed =
            docs_block_text(r#"[<u>A2\](B</u>](https://example.test/label-delimiter) link target"#);

        assert_eq!(parsed.text, "A2](B link target\n");
        assert!(
            parsed.style_ranges.iter().any(|range| {
                range.start == 0
                    && range.end == docs_text_len("A2](B")
                    && range.style.link.as_deref() == Some("https://example.test/label-delimiter")
            }),
            "expected escaped Markdown label delimiter to stay inside one link label: {:#?}",
            parsed.style_ranges
        );
        assert!(
            parsed.style_ranges.iter().any(|range| {
                range.start == 0 && range.end == docs_text_len("A2](B") && range.style.underline
            }),
            "expected nested underline to remain scoped to the complete linked label: {:#?}",
            parsed.style_ranges
        );
    }

    #[test]
    fn apply_decodes_escaped_literal_markdown_block_markers() {
        for (escaped, literal) in [
            ("\\# Literal heading", "# Literal heading\n"),
            ("\\- Literal bullet", "- Literal bullet\n"),
            ("\\1. Literal number", "1. Literal number\n"),
            ("\\> Literal quote", "> Literal quote\n"),
            ("\\---", "---\n"),
            (
                "\\::loc{id=literal type=paragraph}",
                "::loc{id=literal type=paragraph}\n",
            ),
        ] {
            let parsed = docs_block_text(escaped);
            assert_eq!(parsed.text, literal, "escaped block marker {escaped:?}");
            assert!(
                parsed.bullet_ranges.is_empty(),
                "escaped block marker must not create bullets: {escaped:?}"
            );
            assert!(
                parsed
                    .paragraph_styles
                    .iter()
                    .all(|range| range.named_style_type == "NORMAL_TEXT"),
                "escaped block marker must remain a normal paragraph: {escaped:?}"
            );
        }
    }

    #[test]
    fn apply_converts_nested_markdown_list_indent_to_docs_tabs() {
        let nested_bullet = docs_block_text("  - Nested bullet");
        assert_eq!(nested_bullet.text, "\tNested bullet\n");
        assert_eq!(nested_bullet.bullet_ranges.len(), 1);
        assert_eq!(nested_bullet.bullet_ranges[0].start, 0);
        assert_eq!(
            nested_bullet.bullet_ranges[0].end,
            docs_text_len("\tNested bullet\n")
        );
        assert_eq!(
            nested_bullet.bullet_ranges[0].preset,
            "BULLET_DISC_CIRCLE_SQUARE"
        );

        let double_nested_number = docs_block_text("    1. Nested number");
        assert_eq!(double_nested_number.text, "\t\tNested number\n");
        assert_eq!(double_nested_number.bullet_ranges.len(), 1);
        assert_eq!(
            double_nested_number.bullet_ranges[0].end,
            docs_text_len("\t\tNested number\n")
        );
        assert_eq!(
            double_nested_number.bullet_ranges[0].preset,
            "NUMBERED_DECIMAL_ALPHA_ROMAN"
        );
    }

    #[test]
    fn document_text_groups_adjacent_nested_bullets_in_one_create_request() {
        let requests = docs_document_text_requests(
            1,
            "- Parent bullet\n\n  - Child bullet\n\n    1. Grandchild number",
        );
        let bullet_requests = requests
            .iter()
            .filter_map(|request| match request {
                DocsRequest::CreateParagraphBullets {
                    create_paragraph_bullets,
                } => Some(create_paragraph_bullets),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(bullet_requests.len(), 2);
        assert_eq!(
            bullet_requests[0].bullet_preset,
            "BULLET_DISC_CIRCLE_SQUARE"
        );
        assert_eq!(bullet_requests[0].range.start_index, 1);
        assert_eq!(
            bullet_requests[0].range.end_index,
            1 + docs_text_len("Parent bullet\n\tChild bullet\n")
        );
        assert_eq!(
            bullet_requests[1].bullet_preset,
            "NUMBERED_DECIMAL_ALPHA_ROMAN"
        );
        assert_eq!(
            bullet_requests[1].range.start_index,
            1 + docs_text_len("Parent bullet\n\tChild bullet\n")
        );
    }

    #[test]
    fn apply_converts_markdown_inline_styles_beyond_bold_to_docs_text() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Pet Resume", "workspace")));
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "doc-1",
            "Pet Resume",
            "rev-1",
            "Styled: Bold Italic Under Strike Link Plain\n",
        )));
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:45"),
                content: "Styled: **Bold** *Italic* <u>Under</u> ~~Strike~~ [<u>Link</u>](https://example.test/live-inline) Plain edited".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let DocsRequest::InsertText { insert_text } = &batch.requests[1] else {
            panic!("expected insert text request");
        };
        assert_eq!(
            insert_text.text,
            "Styled: Bold Italic Under Strike Link Plain edited"
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[4]).expect("paragraph style json"),
            serde_json::json!({
                "updateParagraphStyle": {
                    "range": { "startIndex": 1, "endIndex": 51 },
                    "paragraphStyle": {
                        "namedStyleType": "NORMAL_TEXT"
                    },
                    "fields": "namedStyleType"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[5]).expect("bold style json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 9, "endIndex": 13 },
                    "textStyle": { "bold": true },
                    "fields": "bold"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[6]).expect("italic style json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 14, "endIndex": 20 },
                    "textStyle": { "italic": true },
                    "fields": "italic"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[7]).expect("underline style json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 21, "endIndex": 26 },
                    "textStyle": { "underline": true },
                    "fields": "underline"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[8]).expect("strike style json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 27, "endIndex": 33 },
                    "textStyle": { "strikethrough": true },
                    "fields": "strikethrough"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[9]).expect("link underline style json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 34, "endIndex": 38 },
                    "textStyle": { "underline": true },
                    "fields": "underline"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[10]).expect("link style json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 34, "endIndex": 38 },
                    "textStyle": { "link": { "url": "https://example.test/live-inline" } },
                    "fields": "link"
                }
            })
        );
    }

    #[test]
    fn apply_resets_inherited_style_outside_markdown_inline_span() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Pet Resume", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Pet Resume",
                    "revisionId": "rev-1",
                    "body": {
                        "content": [{
                            "startIndex": 1,
                            "endIndex": 14,
                            "paragraph": {
                                "elements": [
                                    {
                                        "startIndex": 1,
                                        "endIndex": 5,
                                        "textRun": {
                                            "content": "Age:",
                                            "textStyle": {
                                                "bold": true,
                                                "foregroundColor": {
                                                    "color": {
                                                        "rgbColor": {
                                                            "green": 0.67058825,
                                                            "blue": 0.26666668
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    },
                                    {
                                        "startIndex": 5,
                                        "endIndex": 14,
                                        "textRun": {
                                            "content": " 4 years\n",
                                            "textStyle": {}
                                        }
                                    }
                                ]
                            }
                        }]
                    }
                }))
                .expect("styled document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:14"),
                content: "**Age**: 5 years".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        assert_eq!(batch.requests.len(), 6);
        assert_eq!(
            serde_json::to_value(&batch.requests[2]).expect("style reset json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 1, "endIndex": 13 },
                    "textStyle": {
                        "bold": false,
                        "italic": false,
                        "underline": false,
                        "strikethrough": false,
                        "smallCaps": false,
                        "baselineOffset": "NONE"
                    },
                    "fields": "bold,italic,underline,strikethrough,smallCaps,foregroundColor,backgroundColor,baselineOffset,fontSize,weightedFontFamily,link"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[4]).expect("paragraph style json"),
            serde_json::json!({
                "updateParagraphStyle": {
                    "range": { "startIndex": 1, "endIndex": 13 },
                    "paragraphStyle": {
                        "namedStyleType": "NORMAL_TEXT"
                    },
                    "fields": "namedStyleType"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[5]).expect("age style json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 1, "endIndex": 4 },
                    "textStyle": {
                        "bold": true,
                        "foregroundColor": {
                            "color": {
                                "rgbColor": {
                                    "green": 0.67058825,
                                    "blue": 0.26666668
                                }
                            }
                        }
                    },
                    "fields": "bold,foregroundColor"
                }
            })
        );
    }

    #[test]
    fn apply_restores_inline_styles_after_paragraph_style_reset() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Pet Resume", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Pet Resume",
                    "revisionId": "rev-1",
                    "body": {
                        "content": [{
                            "startIndex": 1,
                            "endIndex": 14,
                            "paragraph": {
                                "elements": [
                                    {
                                        "startIndex": 1,
                                        "endIndex": 5,
                                        "textRun": {
                                            "content": "Age:",
                                            "textStyle": {
                                                "bold": true,
                                                "foregroundColor": {
                                                    "color": {
                                                        "rgbColor": {
                                                            "green": 0.67058825,
                                                            "blue": 0.26666668
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    },
                                    {
                                        "startIndex": 5,
                                        "endIndex": 14,
                                        "textRun": {
                                            "content": " 4 years\n",
                                            "textStyle": {}
                                        }
                                    }
                                ]
                            }
                        }]
                    }
                }))
                .expect("styled document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:14"),
                content: "**Age:** 5 years".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        assert!(
            matches!(
                batch.requests.last(),
                Some(DocsRequest::UpdateTextStyle { update_text_style })
                    if update_text_style.range.start_index == 1
                        && update_text_style.range.end_index == 5
                        && update_text_style.text_style.bold == Some(true)
                        && update_text_style.text_style.foreground_color.is_some()
                        && update_text_style.fields == "bold,foregroundColor"
            ),
            "the inline style restore must be the final style-affecting request: {:#?}",
            batch.requests
        );
    }

    #[test]
    fn apply_preserves_color_only_text_style_for_edited_span() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Status", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Status",
                    "revisionId": "rev-1",
                    "body": {
                        "content": [{
                            "startIndex": 1,
                            "endIndex": 15,
                            "paragraph": {
                                "elements": [
                                    {
                                        "startIndex": 1,
                                        "endIndex": 9,
                                        "textRun": {
                                            "content": "Status: ",
                                            "textStyle": {}
                                        }
                                    },
                                    {
                                        "startIndex": 9,
                                        "endIndex": 14,
                                        "textRun": {
                                            "content": "Green",
                                            "textStyle": {
                                                "foregroundColor": {
                                                    "color": {
                                                        "rgbColor": {
                                                            "green": 0.6,
                                                            "blue": 0.2
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    },
                                    {
                                        "startIndex": 14,
                                        "endIndex": 15,
                                        "textRun": {
                                            "content": "\n",
                                            "textStyle": {}
                                        }
                                    }
                                ]
                            }
                        }]
                    }
                }))
                .expect("styled document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:15"),
                content: "Status: Emerald".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        assert!(
            batch.requests.iter().any(|request| {
                matches!(
                    request,
                    DocsRequest::UpdateTextStyle { update_text_style }
                        if update_text_style.range.start_index == 9
                            && update_text_style.range.end_index == 16
                            && update_text_style.text_style.foreground_color.is_some()
                            && update_text_style.fields == "foregroundColor"
                )
            }),
            "color-only source style should be restored onto the edited span: {:#?}",
            batch.requests
        );
    }

    #[test]
    fn apply_does_not_extend_color_only_style_to_appended_plain_text() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Status", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Status",
                    "revisionId": "rev-1",
                    "body": {
                        "content": [{
                            "startIndex": 1,
                            "endIndex": 15,
                            "paragraph": {
                                "elements": [
                                    {
                                        "startIndex": 1,
                                        "endIndex": 9,
                                        "textRun": {
                                            "content": "Status: ",
                                            "textStyle": {}
                                        }
                                    },
                                    {
                                        "startIndex": 9,
                                        "endIndex": 14,
                                        "textRun": {
                                            "content": "Green",
                                            "textStyle": {
                                                "foregroundColor": {
                                                    "color": {
                                                        "rgbColor": {
                                                            "green": 0.6,
                                                            "blue": 0.2
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    },
                                    {
                                        "startIndex": 14,
                                        "endIndex": 15,
                                        "textRun": {
                                            "content": "\n",
                                            "textStyle": {}
                                        }
                                    }
                                ]
                            }
                        }]
                    }
                }))
                .expect("styled document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:15"),
                content: "Status: Green today".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        assert!(
            batch.requests.iter().any(|request| {
                matches!(
                    request,
                    DocsRequest::UpdateTextStyle { update_text_style }
                        if update_text_style.range.start_index == 9
                            && update_text_style.range.end_index == 14
                            && update_text_style.text_style.foreground_color.is_some()
                            && update_text_style.fields == "foregroundColor"
                )
            }),
            "appended plain text after a colored span must remain uncolored: {:#?}",
            batch.requests
        );
    }

    #[test]
    fn apply_preserves_background_color_only_text_style_for_edited_span() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Highlight", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Highlight",
                    "revisionId": "rev-1",
                    "body": {
                        "content": [{
                            "startIndex": 1,
                            "endIndex": 19,
                            "paragraph": {
                                "elements": [
                                    {
                                        "startIndex": 1,
                                        "endIndex": 12,
                                        "textRun": {
                                            "content": "Highlight: ",
                                            "textStyle": {}
                                        }
                                    },
                                    {
                                        "startIndex": 12,
                                        "endIndex": 18,
                                        "textRun": {
                                            "content": "Yellow",
                                            "textStyle": {
                                                "backgroundColor": {
                                                    "color": {
                                                        "rgbColor": {
                                                            "red": 1.0,
                                                            "green": 0.9019608,
                                                            "blue": 0.2
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    },
                                    {
                                        "startIndex": 18,
                                        "endIndex": 19,
                                        "textRun": {
                                            "content": "\n",
                                            "textStyle": {}
                                        }
                                    }
                                ]
                            }
                        }]
                    }
                }))
                .expect("highlighted document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:19"),
                content: "Highlight: Amber".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        assert!(
            batch.requests.iter().any(|request| {
                let value = serde_json::to_value(request).expect("request json");
                value["updateTextStyle"]["range"]["startIndex"] == 12
                    && value["updateTextStyle"]["range"]["endIndex"] == 17
                    && value["updateTextStyle"]["textStyle"]["backgroundColor"].is_object()
                    && value["updateTextStyle"]["fields"] == "backgroundColor"
            }),
            "background-only source style should be restored onto the edited span: {:#?}",
            batch.requests
        );
    }

    #[test]
    fn apply_preserves_baseline_offset_only_text_style_for_edited_span() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Formula", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Formula",
                    "revisionId": "rev-1",
                    "body": {
                        "content": [{
                            "startIndex": 1,
                            "endIndex": 18,
                            "paragraph": {
                                "elements": [
                                    {
                                        "startIndex": 1,
                                        "endIndex": 11,
                                        "textRun": {
                                            "content": "Formula: x",
                                            "textStyle": {}
                                        }
                                    },
                                    {
                                        "startIndex": 11,
                                        "endIndex": 12,
                                        "textRun": {
                                            "content": "2",
                                            "textStyle": {
                                                "baselineOffset": "SUPERSCRIPT"
                                            }
                                        }
                                    },
                                    {
                                        "startIndex": 12,
                                        "endIndex": 16,
                                        "textRun": {
                                            "content": " + y",
                                            "textStyle": {}
                                        }
                                    },
                                    {
                                        "startIndex": 16,
                                        "endIndex": 17,
                                        "textRun": {
                                            "content": "2",
                                            "textStyle": {
                                                "baselineOffset": "SUPERSCRIPT"
                                            }
                                        }
                                    },
                                    {
                                        "startIndex": 17,
                                        "endIndex": 18,
                                        "textRun": {
                                            "content": "\n",
                                            "textStyle": {}
                                        }
                                    }
                                ]
                            }
                        }]
                    }
                }))
                .expect("superscript document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:18"),
                content: "Formula: x3 + y3".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let serialized: Vec<_> = batch
            .requests
            .iter()
            .map(|request| serde_json::to_value(request).expect("request json"))
            .collect();
        assert!(
            serialized.iter().any(|value| {
                value["updateTextStyle"]["range"]["startIndex"] == 11
                    && value["updateTextStyle"]["range"]["endIndex"] == 12
                    && value["updateTextStyle"]["textStyle"]["baselineOffset"] == "SUPERSCRIPT"
                    && value["updateTextStyle"]["fields"] == "baselineOffset"
            }),
            "first superscript source style should be restored onto the edited exponent: {serialized:#?}"
        );
        assert!(
            serialized.iter().any(|value| {
                value["updateTextStyle"]["range"]["startIndex"] == 16
                    && value["updateTextStyle"]["range"]["endIndex"] == 17
                    && value["updateTextStyle"]["textStyle"]["baselineOffset"] == "SUPERSCRIPT"
                    && value["updateTextStyle"]["fields"] == "baselineOffset"
            }),
            "second superscript source style should be restored onto the edited exponent: {serialized:#?}"
        );
    }

    #[test]
    fn apply_preserves_font_size_only_text_style_for_edited_span() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Sized", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Sized",
                    "revisionId": "rev-1",
                    "body": {
                        "content": [{
                            "startIndex": 1,
                            "endIndex": 13,
                            "paragraph": {
                                "elements": [
                                    {
                                        "startIndex": 1,
                                        "endIndex": 7,
                                        "textRun": {
                                            "content": "Size: ",
                                            "textStyle": {}
                                        }
                                    },
                                    {
                                        "startIndex": 7,
                                        "endIndex": 12,
                                        "textRun": {
                                            "content": "Large",
                                            "textStyle": {
                                                "fontSize": {
                                                    "magnitude": 24,
                                                    "unit": "PT"
                                                }
                                            }
                                        }
                                    },
                                    {
                                        "startIndex": 12,
                                        "endIndex": 13,
                                        "textRun": {
                                            "content": "\n",
                                            "textStyle": {}
                                        }
                                    }
                                ]
                            }
                        }]
                    }
                }))
                .expect("sized document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:13"),
                content: "Size: Huge".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let serialized: Vec<_> = batch
            .requests
            .iter()
            .map(|request| serde_json::to_value(request).expect("request json"))
            .collect();
        assert!(
            serialized.iter().any(|value| {
                value["updateTextStyle"]["range"]["startIndex"] == 7
                    && value["updateTextStyle"]["range"]["endIndex"] == 11
                    && value["updateTextStyle"]["textStyle"]["fontSize"]["magnitude"] == 24
                    && value["updateTextStyle"]["fields"] == "fontSize"
            }),
            "font-size-only source style should be restored onto the edited span: {serialized:#?}"
        );
    }

    #[test]
    fn apply_preserves_font_family_only_text_style_for_edited_span() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Mono", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Mono",
                    "revisionId": "rev-1",
                    "body": {
                        "content": [{
                            "startIndex": 1,
                            "endIndex": 13,
                            "paragraph": {
                                "elements": [
                                    {
                                        "startIndex": 1,
                                        "endIndex": 7,
                                        "textRun": {
                                            "content": "Font: ",
                                            "textStyle": {}
                                        }
                                    },
                                    {
                                        "startIndex": 7,
                                        "endIndex": 12,
                                        "textRun": {
                                            "content": "Mono\n",
                                            "textStyle": {
                                                "weightedFontFamily": {
                                                    "fontFamily": "Courier New",
                                                    "weight": 400
                                                }
                                            }
                                        }
                                    }
                                ]
                            }
                        }]
                    }
                }))
                .expect("font family document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:13"),
                content: "Font: Code".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let serialized: Vec<_> = batch
            .requests
            .iter()
            .map(|request| serde_json::to_value(request).expect("request json"))
            .collect();
        assert!(
            serialized.iter().any(|value| {
                value["updateTextStyle"]["range"]["startIndex"] == 7
                    && value["updateTextStyle"]["range"]["endIndex"] == 11
                    && value["updateTextStyle"]["textStyle"]["weightedFontFamily"]["fontFamily"]
                        == "Courier New"
                    && value["updateTextStyle"]["textStyle"]["weightedFontFamily"]["weight"] == 400
                    && value["updateTextStyle"]["fields"] == "weightedFontFamily"
            }),
            "font-family-only source style should be restored onto the edited span: {serialized:#?}"
        );
    }

    #[test]
    fn apply_preserves_small_caps_only_text_style_for_edited_span() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Caps", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Caps",
                    "revisionId": "rev-1",
                    "body": {
                        "content": [{
                            "startIndex": 1,
                            "endIndex": 13,
                            "paragraph": {
                                "elements": [
                                    {
                                        "startIndex": 1,
                                        "endIndex": 7,
                                        "textRun": {
                                            "content": "Caps: ",
                                            "textStyle": {}
                                        }
                                    },
                                    {
                                        "startIndex": 7,
                                        "endIndex": 12,
                                        "textRun": {
                                            "content": "Word\n",
                                            "textStyle": {
                                                "smallCaps": true
                                            }
                                        }
                                    }
                                ]
                            }
                        }]
                    }
                }))
                .expect("small caps document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:13"),
                content: "Caps: Term".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let serialized: Vec<_> = batch
            .requests
            .iter()
            .map(|request| serde_json::to_value(request).expect("request json"))
            .collect();
        assert!(
            serialized.iter().any(|value| {
                value["updateTextStyle"]["range"]["startIndex"] == 7
                    && value["updateTextStyle"]["range"]["endIndex"] == 11
                    && value["updateTextStyle"]["textStyle"]["smallCaps"] == true
                    && value["updateTextStyle"]["fields"] == "smallCaps"
            }),
            "small-caps-only source style should be restored onto the edited span: {serialized:#?}"
        );
    }

    #[test]
    fn apply_preserves_paragraph_alignment_for_edited_block() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Aligned", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Aligned",
                    "revisionId": "rev-1",
                    "body": {
                        "content": [{
                            "startIndex": 1,
                            "endIndex": 15,
                            "paragraph": {
                                "paragraphStyle": {
                                    "namedStyleType": "NORMAL_TEXT",
                                    "alignment": "CENTER"
                                },
                                "elements": [{
                                    "startIndex": 1,
                                    "endIndex": 15,
                                    "textRun": {
                                        "content": "Centered line\n",
                                        "textStyle": {}
                                    }
                                }]
                            }
                        }]
                    }
                }))
                .expect("centered document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:15"),
                content: "Centered phrase".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let serialized: Vec<_> = batch
            .requests
            .iter()
            .map(|request| serde_json::to_value(request).expect("request json"))
            .collect();
        assert!(
            serialized.iter().any(|value| {
                value["updateParagraphStyle"]["range"]["startIndex"] == 1
                    && value["updateParagraphStyle"]["range"]["endIndex"] == 16
                    && value["updateParagraphStyle"]["paragraphStyle"]["alignment"] == "CENTER"
                    && value["updateParagraphStyle"]["fields"] == "alignment"
            }),
            "source paragraph alignment should be restored after editing the block: {serialized:#?}"
        );
    }

    #[test]
    fn apply_preserves_paragraph_indentation_for_edited_block() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Indented", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Indented",
                    "revisionId": "rev-1",
                    "body": {
                        "content": [{
                            "startIndex": 1,
                            "endIndex": 20,
                            "paragraph": {
                                "paragraphStyle": {
                                    "namedStyleType": "NORMAL_TEXT",
                                    "indentStart": {
                                        "magnitude": 36,
                                        "unit": "PT"
                                    },
                                    "indentFirstLine": {
                                        "magnitude": 18,
                                        "unit": "PT"
                                    }
                                },
                                "elements": [{
                                    "startIndex": 1,
                                    "endIndex": 20,
                                    "textRun": {
                                        "content": "Indented paragraph\n",
                                        "textStyle": {}
                                    }
                                }]
                            }
                        }]
                    }
                }))
                .expect("indented document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:20"),
                content: "Indented paragraph updated".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let serialized: Vec<_> = batch
            .requests
            .iter()
            .map(|request| serde_json::to_value(request).expect("request json"))
            .collect();
        assert!(
            serialized.iter().any(|value| {
                value["updateParagraphStyle"]["range"]["startIndex"] == 1
                    && value["updateParagraphStyle"]["range"]["endIndex"] == 27
                    && value["updateParagraphStyle"]["paragraphStyle"]["indentStart"]["magnitude"]
                        == 36
                    && value["updateParagraphStyle"]["paragraphStyle"]["indentFirstLine"]
                        ["magnitude"]
                        == 18
                    && value["updateParagraphStyle"]["fields"] == "indentStart,indentFirstLine"
            }),
            "source paragraph indentation should be restored after editing the block: {serialized:#?}"
        );
    }

    #[test]
    fn apply_clears_inherited_bullets_for_non_list_block_updates() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "List Doc", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "List Doc",
                    "revisionId": "rev-1",
                    "lists": {
                        "list-1": {
                            "listProperties": {
                                "nestingLevels": [{ "glyphType": "DECIMAL" }]
                            }
                        }
                    },
                    "body": {
                        "content": [
                            {
                                "startIndex": 1,
                                "endIndex": 12,
                                "paragraph": {
                                    "elements": [{
                                        "startIndex": 1,
                                        "endIndex": 12,
                                        "textRun": { "content": "Intro line\n" }
                                    }]
                                }
                            },
                            {
                                "startIndex": 12,
                                "endIndex": 22,
                                "paragraph": {
                                    "bullet": { "listId": "list-1", "nestingLevel": 0 },
                                    "elements": [{
                                        "startIndex": 12,
                                        "endIndex": 22,
                                        "textRun": { "content": "List item\n" }
                                    }]
                                }
                            }
                        ]
                    }
                }))
                .expect("list document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:12"),
                content: "Intro edited".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        assert!(
            batch.requests.iter().any(|request| {
                serde_json::to_value(request)
                    .expect("request json")
                    .get("deleteParagraphBullets")
                    .is_some()
            }),
            "expected non-list block updates to clear inherited list bullets"
        );
    }

    #[test]
    fn create_entity_preserves_markdown_paragraph_breaks_as_docs_paragraphs() {
        let drive = Arc::new(FakeDrive::default());
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "created-doc",
            "Local Shape Create",
            "rev-1",
            "",
        )));
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("workspace")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("workspace"),
                parent_kind: Some(EntityKind::Directory),
                title: "Local Shape Create".to_string(),
                properties: BTreeMap::new(),
                body: "# Local Shape Create\n\nIntro paragraph\n".to_string(),
                source_path: PathBuf::from("local-shape-create/page.md"),
            }],
        );
        let op_ids = vec![PushOperationId(
            "push-1:0:create_entity:workspace".to_string(),
        )];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let DocsRequest::InsertText { insert_text } = &batch.requests[0] else {
            panic!("expected insert text request");
        };
        assert_eq!(insert_text.text, "Local Shape Create\nIntro paragraph\n");
        assert!(
            !insert_text.text.contains('\u{000b}'),
            "full-document creates must not collapse Markdown blocks into soft breaks"
        );
    }

    #[test]
    fn create_entity_converts_markdown_blocks_to_docs_paragraph_styles_and_lists() {
        let drive = Arc::new(FakeDrive::default());
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "created-doc",
            "Local Shape Create",
            "rev-1",
            "",
        )));
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("workspace")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("workspace"),
                parent_kind: Some(EntityKind::Directory),
                title: "Local Shape Create".to_string(),
                properties: BTreeMap::new(),
                body: "# Local Shape Create\n\nIntro with **Bold** and *Italic*.\n\n## Section Two\n\n- Bullet alpha\n\n1. Number alpha\n".to_string(),
                source_path: PathBuf::from("local-shape-create/page.md"),
            }],
        );
        let op_ids = vec![PushOperationId(
            "push-1:0:create_entity:workspace".to_string(),
        )];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let DocsRequest::InsertText { insert_text } = &batch.requests[0] else {
            panic!("expected insert text request");
        };
        assert_eq!(
            insert_text.text,
            "Local Shape Create\nIntro with Bold and Italic.\nSection Two\nBullet alpha\nNumber alpha\n"
        );
        assert!(
            batch.requests.iter().any(|request| {
                serde_json::to_value(request)
                    .expect("request json")
                    .pointer("/updateParagraphStyle/paragraphStyle/namedStyleType")
                    == Some(&serde_json::Value::String("HEADING_1".to_string()))
            }),
            "expected heading 1 paragraph style update"
        );
        assert!(
            batch.requests.iter().any(|request| {
                serde_json::to_value(request)
                    .expect("request json")
                    .pointer("/updateParagraphStyle/paragraphStyle/namedStyleType")
                    == Some(&serde_json::Value::String("HEADING_2".to_string()))
            }),
            "expected heading 2 paragraph style update"
        );
        assert!(
            batch.requests.iter().any(|request| {
                serde_json::to_value(request)
                    .expect("request json")
                    .pointer("/createParagraphBullets/bulletPreset")
                    == Some(&serde_json::Value::String(
                        "BULLET_DISC_CIRCLE_SQUARE".to_string(),
                    ))
            }),
            "expected unordered list bullet creation"
        );
        assert!(
            batch.requests.iter().any(|request| {
                serde_json::to_value(request)
                    .expect("request json")
                    .pointer("/createParagraphBullets/bulletPreset")
                    == Some(&serde_json::Value::String(
                        "NUMBERED_DECIMAL_ALPHA_ROMAN".to_string(),
                    ))
            }),
            "expected ordered list bullet creation"
        );
    }

    #[test]
    fn update_block_converts_markdown_heading_and_list_markers_to_docs_shape() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Shape Doc", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Shape Doc",
                    "revisionId": "rev-1",
                    "lists": {
                        "bullets": {
                            "listProperties": {
                                "nestingLevels": [{ "glyphType": "BULLET" }]
                            }
                        }
                    },
                    "body": {
                        "content": [
                            {
                                "startIndex": 1,
                                "endIndex": 13,
                                "paragraph": {
                                    "paragraphStyle": { "namedStyleType": "HEADING_2" },
                                    "elements": [{
                                        "startIndex": 1,
                                        "endIndex": 13,
                                        "textRun": { "content": "Section Two\n" }
                                    }]
                                }
                            },
                            {
                                "startIndex": 13,
                                "endIndex": 26,
                                "paragraph": {
                                    "bullet": { "listId": "bullets", "nestingLevel": 0 },
                                    "elements": [{
                                        "startIndex": 13,
                                        "endIndex": 26,
                                        "textRun": { "content": "Bullet alpha\n" }
                                    }]
                                }
                            }
                        ]
                    }
                }))
                .expect("shape document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![
                PushOperation::UpdateBlock {
                    block_id: RemoteId::new("doc-1:1:13"),
                    content: "## Section Two Edited".to_string(),
                },
                PushOperation::UpdateBlock {
                    block_id: RemoteId::new("doc-1:13:26"),
                    content: "- Bullet alpha edited".to_string(),
                },
            ],
        );
        let op_ids = vec![
            PushOperationId("push-1:0:update_block:doc-1".to_string()),
            PushOperationId("push-1:1:update_block:doc-1".to_string()),
        ];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batches = docs.batches.lock().unwrap();
        let heading_batch = &batches[1].1;
        let DocsRequest::InsertText { insert_text } = &heading_batch.requests[1] else {
            panic!("expected heading insert text request");
        };
        assert_eq!(insert_text.text, "Section Two Edited\n");
        assert!(
            heading_batch.requests.iter().any(|request| {
                serde_json::to_value(request)
                    .expect("request json")
                    .pointer("/updateParagraphStyle/paragraphStyle/namedStyleType")
                    == Some(&serde_json::Value::String("HEADING_2".to_string()))
            }),
            "expected heading update to preserve heading style without literal marker"
        );

        let list_batch = &batches[0].1;
        let DocsRequest::InsertText { insert_text } = &list_batch.requests[1] else {
            panic!("expected list insert text request");
        };
        assert_eq!(insert_text.text, "Bullet alpha edited");
        assert!(
            list_batch.requests.iter().any(|request| {
                serde_json::to_value(request)
                    .expect("request json")
                    .pointer("/createParagraphBullets/bulletPreset")
                    == Some(&serde_json::Value::String(
                        "BULLET_DISC_CIRCLE_SQUARE".to_string(),
                    ))
            }),
            "expected list update to preserve bullet shape without literal marker"
        );
    }

    #[test]
    fn update_block_clears_heading_style_when_markdown_heading_marker_is_removed() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Shape Doc", "workspace")));
        let docs = Arc::new(
            FakeDocs::default().with_document(
                serde_json::from_value(serde_json::json!({
                    "documentId": "doc-1",
                    "title": "Shape Doc",
                    "revisionId": "rev-1",
                    "body": {
                        "content": [
                            {
                                "startIndex": 1,
                                "endIndex": 13,
                                "paragraph": {
                                    "paragraphStyle": { "namedStyleType": "HEADING_2" },
                                    "elements": [{
                                        "startIndex": 1,
                                        "endIndex": 13,
                                        "textRun": { "content": "Old Heading\n" }
                                    }]
                                }
                            }
                        ]
                    }
                }))
                .expect("heading document"),
            ),
        );
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:13"),
                content: "Plain paragraph now".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        assert!(
            batch.requests.iter().any(|request| {
                serde_json::to_value(request)
                    .expect("request json")
                    .pointer("/updateParagraphStyle/paragraphStyle/namedStyleType")
                    == Some(&serde_json::Value::String("NORMAL_TEXT".to_string()))
            }),
            "expected plain Markdown block to clear existing heading style"
        );
    }

    #[test]
    fn update_final_block_preserves_google_docs_segment_newline() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Final Doc", "workspace")));
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "doc-1",
            "Final Doc",
            "rev-1",
            "Original\n",
        )));
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::UpdateBlock {
                block_id: RemoteId::new("doc-1:1:10"),
                content: "Updated".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:update_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let delete_range = first_delete_range(&batch);
        assert_eq!(delete_range.start_index, 1);
        assert_eq!(delete_range.end_index, 9);
        let DocsRequest::InsertText { insert_text } = &batch.requests[1] else {
            panic!("expected insert text request");
        };
        assert_eq!(insert_text.text, "Updated");
    }

    #[test]
    fn append_block_converts_markdown_heading_and_list_markers_to_docs_shape() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Shape Doc", "workspace")));
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "doc-1",
            "Shape Doc",
            "rev-1",
            "Intro\n",
        )));
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![
                PushOperation::AppendBlock {
                    parent_id: RemoteId::new("doc-1"),
                    after: None,
                    content: "## Appended Section".to_string(),
                },
                PushOperation::AppendBlock {
                    parent_id: RemoteId::new("doc-1"),
                    after: None,
                    content: "- Appended bullet".to_string(),
                },
                PushOperation::AppendBlock {
                    parent_id: RemoteId::new("doc-1"),
                    after: None,
                    content: "1. Appended number".to_string(),
                },
            ],
        );
        let op_ids = vec![
            PushOperationId("push-1:0:append_block:doc-1".to_string()),
            PushOperationId("push-1:1:append_block:doc-1".to_string()),
            PushOperationId("push-1:2:append_block:doc-1".to_string()),
        ];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batches = docs.batches.lock().unwrap();
        let heading_batch = &batches[0].1;
        let DocsRequest::InsertText { insert_text } = &heading_batch.requests[0] else {
            panic!("expected heading insert text request");
        };
        assert_eq!(insert_text.location.index, 1);
        assert_eq!(insert_text.text, "Appended Section\n");
        assert!(
            heading_batch.requests.iter().any(|request| {
                serde_json::to_value(request)
                    .expect("request json")
                    .pointer("/updateParagraphStyle/paragraphStyle/namedStyleType")
                    == Some(&serde_json::Value::String("HEADING_2".to_string()))
            }),
            "expected appended heading to use paragraph style without literal marker"
        );

        let list_batch = &batches[1].1;
        let DocsRequest::InsertText { insert_text } = &list_batch.requests[0] else {
            panic!("expected list insert text request");
        };
        assert_eq!(insert_text.location.index, 18);
        assert_eq!(insert_text.text, "Appended bullet\n");
        assert!(
            list_batch.requests.iter().any(|request| {
                serde_json::to_value(request)
                    .expect("request json")
                    .pointer("/createParagraphBullets/bulletPreset")
                    == Some(&serde_json::Value::String(
                        "BULLET_DISC_CIRCLE_SQUARE".to_string(),
                    ))
            }),
            "expected appended list item to use bullet shape without literal marker"
        );

        let ordered_list_batch = &batches[2].1;
        let DocsRequest::InsertText { insert_text } = &ordered_list_batch.requests[0] else {
            panic!("expected ordered list insert text request");
        };
        assert_eq!(insert_text.location.index, 34);
        assert_eq!(insert_text.text, "Appended number\n");
        assert!(
            ordered_list_batch.requests.iter().any(|request| {
                serde_json::to_value(request)
                    .expect("request json")
                    .pointer("/createParagraphBullets/bulletPreset")
                    == Some(&serde_json::Value::String(
                        "NUMBERED_DECIMAL_ALPHA_ROMAN".to_string(),
                    ))
            }),
            "expected appended ordered list item to use numbered shape without literal marker"
        );
    }

    #[test]
    fn append_block_without_after_inserts_before_first_document_block() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Shape Doc", "workspace")));
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "doc-1",
            "Shape Doc",
            "rev-1",
            "Intro\n",
        )));
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![PushOperation::AppendBlock {
                parent_id: RemoteId::new("doc-1"),
                after: None,
                content: "Before intro".to_string(),
            }],
        );
        let op_ids = vec![PushOperationId("push-1:0:append_block:doc-1".to_string())];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batch = docs
            .last_batch
            .lock()
            .unwrap()
            .clone()
            .expect("batch update");
        let DocsRequest::InsertText { insert_text } = &batch.requests[0] else {
            panic!("expected insert text request");
        };
        assert_eq!(insert_text.location.index, 1);
        assert_eq!(insert_text.text, "Before intro\n");
    }

    #[test]
    fn archive_after_insert_before_block_uses_shifted_range() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Shape Doc", "workspace")));
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "doc-1",
            "Shape Doc",
            "rev-1",
            "Intro\n",
        )));
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![
                PushOperation::AppendBlock {
                    parent_id: RemoteId::new("doc-1"),
                    after: None,
                    content: "Before".to_string(),
                },
                PushOperation::ArchiveBlock {
                    block_id: RemoteId::new("doc-1:10:16"),
                },
            ],
        );
        let op_ids = vec![
            PushOperationId("push-1:0:append_block:doc-1".to_string()),
            PushOperationId("push-1:1:archive_block:doc-1:10:16".to_string()),
        ];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batches = docs.batches.lock().unwrap();
        let shifted_delete = first_delete_range(&batches[1].1);
        assert_eq!(shifted_delete.start_index, 17);
        assert_eq!(shifted_delete.end_index, 23);
    }

    #[test]
    fn apply_orders_same_document_block_updates_from_bottom_to_top() {
        let drive =
            Arc::new(FakeDrive::default().with_file(doc_file("doc-1", "Pet Resume", "workspace")));
        let docs = Arc::new(FakeDocs::default().with_document(document(
            "doc-1",
            "Pet Resume",
            "rev-1",
            "First block\nSecond block\n",
        )));
        let connector =
            GoogleDocsConnector::with_apis(GoogleDocsConfig::new("token"), drive, docs.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("doc-1")],
            vec![
                PushOperation::UpdateBlock {
                    block_id: RemoteId::new("doc-1:1:13"),
                    content: "First".to_string(),
                },
                PushOperation::UpdateBlock {
                    block_id: RemoteId::new("doc-1:13:26"),
                    content: "Second".to_string(),
                },
            ],
        );
        let op_ids = vec![
            PushOperationId("push-1:0:update_block:doc-1".to_string()),
            PushOperationId("push-1:1:update_block:doc-1".to_string()),
        ];

        connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect("apply");

        let batches = docs.batches.lock().unwrap();
        let first_delete = first_delete_range(&batches[0].1);
        let second_delete = first_delete_range(&batches[1].1);
        assert_eq!(first_delete.start_index, 13);
        assert_eq!(second_delete.start_index, 1);
    }

    #[test]
    fn concurrency_skips_preconditions_without_synced_remote_version() {
        let drive =
            Arc::new(FakeDrive::default().with_file(folder("workspace", "Locality", "root")));
        let connector = GoogleDocsConnector::with_apis(
            GoogleDocsConfig::new("token"),
            drive,
            Arc::new(FakeDocs::default()),
        );
        let plan = PushPlan::new(
            vec![RemoteId::new("workspace")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("workspace"),
                parent_kind: Some(EntityKind::Directory),
                title: "Scratch Hydration".to_string(),
                properties: BTreeMap::new(),
                body: "Created locally.\n".to_string(),
                source_path: PathBuf::from("scratch-hydration/page.md"),
            }],
        );
        let op_ids = vec![PushOperationId(
            "push-1:0:create_entity:workspace".to_string(),
        )];
        let preconditions = vec![RemotePrecondition {
            remote_id: RemoteId::new("workspace"),
            remote_edited_at: None,
        }];

        connector
            .check_concurrency(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &preconditions,
                local_root: None,
            })
            .expect("concurrency");
    }

    #[test]
    fn apply_trashes_created_doc_when_body_insert_fails() {
        let drive = Arc::new(FakeDrive::default());
        let connector = GoogleDocsConnector::with_apis(
            GoogleDocsConfig::new("token"),
            drive.clone(),
            Arc::new(FakeDocs::default()),
        );
        let plan = PushPlan::new(
            vec![RemoteId::new("workspace")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("workspace"),
                parent_kind: Some(EntityKind::Directory),
                title: "Scratch Hydration".to_string(),
                properties: BTreeMap::new(),
                body: "Created locally.\n".to_string(),
                source_path: PathBuf::from("scratch-hydration/page.md"),
            }],
        );
        let op_ids = vec![PushOperationId(
            "push-1:0:create_entity:workspace".to_string(),
        )];

        let error = connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("google-docs-main"),
                plan: &plan,
                operation_ids: &op_ids,
                remote_preconditions: &[],
                local_root: None,
            })
            .expect_err("apply should fail");

        assert!(
            matches!(error, locality_core::LocalityError::RemoteNotFound(_)),
            "{error:?}"
        );
        let update = drive
            .last_update
            .lock()
            .unwrap()
            .clone()
            .expect("rollback trash");
        assert_eq!(update.0, "created-doc");
        assert_eq!(update.1.trashed, Some(true));
    }

    #[test]
    fn resolve_workspace_folder_reuses_matching_named_folder() {
        let drive = Arc::new(FakeDrive::default().with_workspace_folders(
            "Locality Workspace",
            vec![folder("folder-1", "Locality Workspace", "root")],
        ));
        let connector = GoogleDocsConnector::with_apis(
            GoogleDocsConfig::new("token"),
            drive.clone(),
            Arc::new(FakeDocs::default()),
        );

        let folder_id = connector
            .resolve_workspace_folder("Locality Workspace")
            .expect("resolve workspace folder");

        assert_eq!(folder_id, RemoteId::new("folder-1"));
        assert!(drive.last_created.lock().unwrap().is_none());
    }

    #[test]
    fn resolve_workspace_folder_creates_missing_named_folder() {
        let drive = Arc::new(FakeDrive::default());
        let connector = GoogleDocsConnector::with_apis(
            GoogleDocsConfig::new("token"),
            drive.clone(),
            Arc::new(FakeDocs::default()),
        );

        let folder_id = connector
            .resolve_workspace_folder("Locality Workspace")
            .expect("create workspace folder");

        assert_eq!(folder_id, RemoteId::new("created-folder"));
        let created = drive.last_created.lock().unwrap().clone().expect("create");
        assert_eq!(created.name, "Locality Workspace");
        assert_eq!(created.mime_type, crate::drive_dto::DRIVE_FOLDER_MIME_TYPE);
    }

    #[derive(Debug, Default)]
    struct FakeDrive {
        files: Mutex<std::collections::BTreeMap<String, DriveFile>>,
        children: Mutex<std::collections::BTreeMap<String, Vec<DriveFile>>>,
        workspace_folders: Mutex<std::collections::BTreeMap<String, Vec<DriveFile>>>,
        last_created: Mutex<Option<DriveCreateFileRequest>>,
        last_update: Mutex<Option<(String, DriveUpdateFileRequest)>>,
    }

    impl FakeDrive {
        fn with_file(self, file: DriveFile) -> Self {
            self.files.lock().unwrap().insert(file.id.clone(), file);
            self
        }

        fn with_children(self, parent: &str, files: Vec<DriveFile>) -> Self {
            for file in &files {
                self.files
                    .lock()
                    .unwrap()
                    .insert(file.id.clone(), file.clone());
            }
            self.children
                .lock()
                .unwrap()
                .insert(parent.to_string(), files);
            self
        }

        fn with_workspace_folders(self, name: &str, files: Vec<DriveFile>) -> Self {
            for file in &files {
                self.files
                    .lock()
                    .unwrap()
                    .insert(file.id.clone(), file.clone());
            }
            self.workspace_folders
                .lock()
                .unwrap()
                .insert(name.to_string(), files);
            self
        }
    }

    impl GoogleDriveApi for FakeDrive {
        fn get_file(&self, file_id: &str) -> locality_core::LocalityResult<DriveFile> {
            self.files
                .lock()
                .unwrap()
                .get(file_id)
                .cloned()
                .ok_or_else(|| locality_core::LocalityError::RemoteNotFound(file_id.to_string()))
        }

        fn list_children(
            &self,
            parent_id: &str,
            _page_token: Option<&str>,
        ) -> locality_core::LocalityResult<DriveFileList> {
            Ok(DriveFileList {
                files: self
                    .children
                    .lock()
                    .unwrap()
                    .get(parent_id)
                    .cloned()
                    .unwrap_or_default(),
                next_page_token: None,
            })
        }

        fn list_workspace_folders_by_name(
            &self,
            name: &str,
            _page_token: Option<&str>,
        ) -> locality_core::LocalityResult<DriveFileList> {
            Ok(DriveFileList {
                files: self
                    .workspace_folders
                    .lock()
                    .unwrap()
                    .get(name)
                    .cloned()
                    .unwrap_or_default(),
                next_page_token: None,
            })
        }

        fn create_file(
            &self,
            request: DriveCreateFileRequest,
        ) -> locality_core::LocalityResult<DriveFile> {
            *self.last_created.lock().unwrap() = Some(request.clone());
            if request.mime_type == crate::drive_dto::DRIVE_FOLDER_MIME_TYPE {
                let created = folder(
                    "created-folder",
                    &request.name,
                    request
                        .parents
                        .first()
                        .map(String::as_str)
                        .unwrap_or("root"),
                );
                self.files
                    .lock()
                    .unwrap()
                    .insert(created.id.clone(), created.clone());
                return Ok(created);
            }
            let created = doc_file(
                "created-doc",
                &request.name,
                request
                    .parents
                    .first()
                    .map(String::as_str)
                    .unwrap_or("workspace"),
            );
            self.files
                .lock()
                .unwrap()
                .insert(created.id.clone(), created.clone());
            Ok(created)
        }

        fn update_file(
            &self,
            file_id: &str,
            request: DriveUpdateFileRequest,
        ) -> locality_core::LocalityResult<DriveFile> {
            *self.last_update.lock().unwrap() = Some((file_id.to_string(), request));
            self.get_file(file_id)
        }
    }

    #[derive(Debug, Default)]
    struct FakeDocs {
        docs: Mutex<std::collections::BTreeMap<String, GoogleDocument>>,
        last_batch: Mutex<Option<BatchUpdateDocumentRequest>>,
        batches: Mutex<Vec<(String, BatchUpdateDocumentRequest)>>,
    }

    impl FakeDocs {
        fn with_document(self, document: GoogleDocument) -> Self {
            self.docs
                .lock()
                .unwrap()
                .insert(document.document_id.clone(), document);
            self
        }
    }

    impl GoogleDocsApi for FakeDocs {
        fn get_document(&self, document_id: &str) -> locality_core::LocalityResult<GoogleDocument> {
            self.docs
                .lock()
                .unwrap()
                .get(document_id)
                .cloned()
                .ok_or_else(|| {
                    locality_core::LocalityError::RemoteNotFound(document_id.to_string())
                })
        }

        fn batch_update_document(
            &self,
            document_id: &str,
            request: BatchUpdateDocumentRequest,
        ) -> locality_core::LocalityResult<GoogleDocument> {
            self.batches
                .lock()
                .unwrap()
                .push((document_id.to_string(), request.clone()));
            *self.last_batch.lock().unwrap() = Some(request);
            self.get_document(document_id)
        }
    }

    fn first_delete_range(request: &BatchUpdateDocumentRequest) -> Range {
        request
            .requests
            .iter()
            .find_map(|request| match request {
                DocsRequest::DeleteContentRange {
                    delete_content_range,
                } => Some(delete_content_range.range.clone()),
                _ => None,
            })
            .expect("delete request")
    }

    fn folder(id: &str, name: &str, parent: &str) -> DriveFile {
        DriveFile {
            id: id.to_string(),
            name: name.to_string(),
            mime_type: crate::drive_dto::DRIVE_FOLDER_MIME_TYPE.to_string(),
            parents: vec![parent.to_string()],
            modified_time: Some("2026-06-25T10:00:00.000Z".to_string()),
            version: Some("7".to_string()),
            trashed: false,
        }
    }

    fn doc_file(id: &str, name: &str, parent: &str) -> DriveFile {
        DriveFile {
            id: id.to_string(),
            name: name.to_string(),
            mime_type: crate::drive_dto::DRIVE_GOOGLE_DOC_MIME_TYPE.to_string(),
            parents: vec![parent.to_string()],
            modified_time: Some("2026-06-25T10:00:00.000Z".to_string()),
            version: Some("7".to_string()),
            trashed: false,
        }
    }

    fn document(id: &str, title: &str, revision: &str, content: &str) -> GoogleDocument {
        serde_json::from_value(serde_json::json!({
            "documentId": id,
            "title": title,
            "revisionId": revision,
            "body": {
                "content": [
                    { "startIndex": 1, "endIndex": content.len() + 1, "paragraph": {
                        "elements": [{ "textRun": { "content": content } }]
                    }}
                ]
            }
        }))
        .expect("document")
    }
}
