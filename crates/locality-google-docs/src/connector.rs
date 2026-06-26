use std::collections::BTreeSet;
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
use locality_core::planner::{PropertyValue, PushOperation, PushOperationKind};
use locality_core::{LocalityError, LocalityResult};

use crate::client::{GoogleDocsApi, GoogleDriveApi, HttpGoogleApiClient};
use crate::docs_dto::{
    BatchUpdateDocumentRequest, DeleteContentRangeRequest, DocsRequest, GoogleDocument,
    InsertTextRequest, Link, Location, Range, TextStyle, TextStylePatch, UpdateTextStyleRequest,
    WriteControl,
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
            if expected != current.as_str() {
                return Err(LocalityError::Conflict(
                    locality_core::conflict::ConflictSummary {
                        remote_id: precondition.remote_id.clone(),
                        path: PathBuf::from(precondition.remote_id.as_str()),
                        remote_path: PathBuf::from(precondition.remote_id.as_str()),
                        reason: locality_core::conflict::ConflictReason::RemoteMovedDuringPush,
                    },
                ));
            }
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
                let document = docs.get_document(&range.document_id)?;
                let mut requests = vec![DocsRequest::DeleteContentRange {
                    delete_content_range: DeleteContentRangeRequest {
                        range: Range {
                            start_index: range.start_index,
                            end_index: range.end_index,
                        },
                    },
                }];
                requests.extend(docs_text_requests_with_style_source(
                    range.start_index,
                    content,
                    Some(DocsTextStyleSource {
                        document: &document,
                        start_index: range.start_index,
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
                let index_position = after
                    .as_ref()
                    .and_then(|after| GoogleBlockRange::parse(after).ok())
                    .map(|range| range.end_index)
                    .unwrap_or_else(|| document_end_index(&document));
                let docs_text = docs_text(content);
                let new_block_end = index_position + docs_text.text.encode_utf16().count();
                let requests = docs_text_requests_from_parsed(index_position, docs_text, None);
                docs.batch_update_document(
                    parent_id.as_str(),
                    BatchUpdateDocumentRequest {
                        requests,
                        write_control: write_control(&document),
                    },
                )?;
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
                            requests: docs_text_requests(1, body),
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

fn docs_text_requests(location_index: usize, content: &str) -> Vec<DocsRequest> {
    docs_text_requests_with_style_source(location_index, content, None)
}

fn docs_text_requests_with_style_source(
    location_index: usize,
    content: &str,
    style_source: Option<DocsTextStyleSource<'_>>,
) -> Vec<DocsRequest> {
    docs_text_requests_from_parsed(location_index, docs_text(content), style_source)
}

fn docs_text_requests_from_parsed(
    location_index: usize,
    docs_text: DocsText,
    style_source: Option<DocsTextStyleSource<'_>>,
) -> Vec<DocsRequest> {
    let inserted_len = docs_text_len(&docs_text.text);
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
    }
    requests.extend(
        docs_text
            .style_ranges
            .into_iter()
            .map(|range| text_style_request(location_index, range, style_source)),
    );
    requests
}

#[derive(Clone, Copy)]
struct DocsTextStyleSource<'a> {
    document: &'a GoogleDocument,
    start_index: usize,
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
                foreground_color: None,
                link: None,
            },
            fields: "bold,italic,underline,strikethrough,foregroundColor,link".to_string(),
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
    if range.style.link.is_some() {
        fields.push("link");
    }
    if foreground_color.is_some() {
        fields.push("foregroundColor");
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
                foreground_color,
                link: range.style.link.map(|url| Link { url: Some(url) }),
            },
            fields: fields.join(","),
        },
    }
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

fn parse_docs_markdown_inline(content: &str) -> DocsText {
    let mut parsed = DocsText::default();
    let mut index = 0;
    while index < content.len() {
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
    let label_close_offset = content[label_start..].find("](")?;
    let label_end = label_start + label_close_offset;
    let url_start = label_end + "](".len();
    let url_close_offset = content[url_start..].find(')')?;
    let url_end = url_start + url_close_offset;
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
            link: Some(content[url_start..url_end].to_string()),
            ..DocsInlineStyle::default()
        },
    );
    Some(url_end + ')'.len_utf8())
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

    use super::{GoogleDocsConfig, GoogleDocsConnector};
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
        let DocsRequest::UpdateTextStyle { update_text_style } = &batch.requests[3] else {
            panic!("expected age style request");
        };
        assert_eq!(update_text_style.range.start_index, 1);
        assert_eq!(update_text_style.range.end_index, 5);
        assert_eq!(update_text_style.text_style.bold, Some(true));
        let DocsRequest::UpdateTextStyle { update_text_style } = &batch.requests[4] else {
            panic!("expected weight style request");
        };
        assert_eq!(update_text_style.range.start_index, 14);
        assert_eq!(update_text_style.range.end_index, 21);
        assert_eq!(update_text_style.text_style.bold, Some(true));
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
            "Styled: Bold Italic Under Strike Link Plain edited\n"
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[3]).expect("bold style json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 9, "endIndex": 13 },
                    "textStyle": { "bold": true },
                    "fields": "bold"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[4]).expect("italic style json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 14, "endIndex": 20 },
                    "textStyle": { "italic": true },
                    "fields": "italic"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[5]).expect("underline style json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 21, "endIndex": 26 },
                    "textStyle": { "underline": true },
                    "fields": "underline"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[6]).expect("strike style json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 27, "endIndex": 33 },
                    "textStyle": { "strikethrough": true },
                    "fields": "strikethrough"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[7]).expect("link underline style json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 34, "endIndex": 38 },
                    "textStyle": { "underline": true },
                    "fields": "underline"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[8]).expect("link style json"),
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
        assert_eq!(batch.requests.len(), 4);
        assert_eq!(
            serde_json::to_value(&batch.requests[2]).expect("style reset json"),
            serde_json::json!({
                "updateTextStyle": {
                    "range": { "startIndex": 1, "endIndex": 14 },
                    "textStyle": {
                        "bold": false,
                        "italic": false,
                        "underline": false,
                        "strikethrough": false
                    },
                    "fields": "bold,italic,underline,strikethrough,foregroundColor,link"
                }
            })
        );
        assert_eq!(
            serde_json::to_value(&batch.requests[3]).expect("age style json"),
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
