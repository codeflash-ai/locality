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
use locality_core::planner::{PushOperation, PushOperationKind, PushPlan};
use locality_core::search::{RAW_SEARCH_METADATA_KEY, SearchMetadata};
use locality_core::{LocalityError, LocalityResult};

use crate::client::{GoogleDocsApi, HttpGoogleApiClient};
use crate::docs_dto::{
    BatchUpdateDocumentRequest, CreateParagraphBulletsRequest, DeleteContentRangeRequest,
    DeleteParagraphBulletsRequest, DocsRequest, GoogleDocument, InsertTextRequest, Link, Location,
    ParagraphStylePatch, Range, TextStyle, TextStylePatch, UpdateParagraphStyleRequest,
    UpdateTextStyleRequest, WriteControl,
};
use crate::oauth::GOOGLE_DOCS_CONNECTOR_ID;
use crate::render::{document_frontmatter, document_remote_version, render_google_document};

#[derive(Clone, PartialEq, Eq)]
pub struct GoogleDocsConfig {
    pub access_token: String,
    pub document_ids: Vec<String>,
}

#[cfg(test)]
mod docs_only_tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use locality_connector::{ApplyPlanRequest, Connector, EnumerateRequest, FetchRequest, ObserveRequest};
    use locality_core::journal::{PushId, PushOperationId};
    use locality_core::model::{MountId, RemoteId};
    use locality_core::planner::{PushOperation, PushPlan};
    use super::{GoogleDocsConfig, GoogleDocsConnector};
    use crate::client::GoogleDocsApi;
    use crate::docs_dto::{BatchUpdateDocumentRequest, GoogleDocument};

    #[derive(Debug, Default)] struct Docs { documents: Mutex<BTreeMap<String, GoogleDocument>>, gets: Mutex<usize>, creates: Mutex<usize>, create_title: Mutex<Option<String>>, batches: Mutex<usize> }
    impl Docs { fn document(self, id: &str, title: &str) -> Self { self.documents.lock().unwrap().insert(id.into(), serde_json::from_value(serde_json::json!({"documentId": id, "title": title, "revisionId": "r1", "body": {"content": []}})).unwrap()); self } }
    impl GoogleDocsApi for Docs {
        fn get_document(&self, id: &str) -> locality_core::LocalityResult<GoogleDocument> { *self.gets.lock().unwrap() += 1; self.documents.lock().unwrap().get(id).cloned().ok_or_else(|| locality_core::LocalityError::RemoteNotFound(id.into())) }
        fn create_document(&self, title: &str) -> locality_core::LocalityResult<GoogleDocument> { *self.creates.lock().unwrap() += 1; *self.create_title.lock().unwrap() = Some(title.into()); Ok(serde_json::from_value(serde_json::json!({"documentId":"created", "title":title, "revisionId":"r1", "body":{"content":[]}})).unwrap()) }
        fn batch_update_document(&self, id: &str, _: BatchUpdateDocumentRequest) -> locality_core::LocalityResult<GoogleDocument> { *self.batches.lock().unwrap() += 1; self.get_document(id).or_else(|_| Ok(serde_json::from_value(serde_json::json!({"documentId":id,"title":"Created","revisionId":"r1","body":{"content":[]}})).unwrap())) }
    }
    #[test] fn selected_documents_are_flat_root_pages() { let docs = Arc::new(Docs::default().document("a", "Zeta").document("b", "Alpha")); let connector = GoogleDocsConnector::with_documents(GoogleDocsConfig::new("t").with_document_ids(vec!["a".into(), "b".into()]), docs); let entries = connector.enumerate(EnumerateRequest { mount_id: MountId::new("m"), cursor: None }).unwrap(); assert_eq!(entries.iter().map(|entry| entry.path.to_string_lossy().to_string()).collect::<Vec<_>>(), ["alpha/page.md", "zeta/page.md"]); }
    #[test] fn fetch_and_observe_reject_unselected_documents_before_docs_get() { let docs = Arc::new(Docs::default().document("a", "Alpha")); let connector = GoogleDocsConnector::with_documents(GoogleDocsConfig::new("t").with_document_ids(vec!["a".into()]), docs.clone()); assert!(connector.fetch(FetchRequest { remote_id: RemoteId::new("outside") }).is_err()); assert!(connector.observe(ObserveRequest { mount_id: MountId::new("m"), remote_id: RemoteId::new("outside") }).is_err()); assert_eq!(*docs.gets.lock().unwrap(), 0); }
    #[test] fn create_posts_title_then_writes_body_and_rejects_drive_operations() { let docs = Arc::new(Docs::default().document("a", "Alpha")); let connector = GoogleDocsConnector::with_documents(GoogleDocsConfig::new("t"), docs.clone()); let create = PushPlan::new(vec![], vec![PushOperation::CreateEntity { parent_id: RemoteId::new("root"), parent_kind: None, parent_workspace: false, title: "New".into(), properties: BTreeMap::new(), body: "Body".into(), source_path: Default::default() }]); let ids = vec![PushOperationId("p:0".into())]; let push = PushId("p".into()); let mount = MountId::new("m"); let result = connector.apply(ApplyPlanRequest { push_id: &push, mount_id: &mount, plan: &create, operation_ids: &ids, remote_preconditions: &[], local_root: None }).unwrap(); assert_eq!(result.changed_remote_ids, [RemoteId::new("created")]); assert!(matches!(result.effects.as_slice(), [locality_core::journal::JournalApplyEffect::CreatedEntity { entity_id, .. }] if entity_id == &RemoteId::new("created"))); assert_eq!(*docs.create_title.lock().unwrap(), Some("New".into())); assert_eq!(*docs.creates.lock().unwrap(), 1); assert_eq!(*docs.batches.lock().unwrap(), 1); let move_plan = PushPlan::new(vec![RemoteId::new("a")], vec![PushOperation::ArchiveEntity { entity_id: RemoteId::new("a") }]); assert!(connector.apply(ApplyPlanRequest { push_id: &push, mount_id: &mount, plan: &move_plan, operation_ids: &ids, remote_preconditions: &[], local_root: None }).is_err()); assert_eq!(*docs.batches.lock().unwrap(), 1); }
    #[test] fn mixed_plan_and_non_root_create_fail_before_any_docs_call() { let docs = Arc::new(Docs::default().document("a", "Alpha")); let connector = GoogleDocsConnector::with_documents(GoogleDocsConfig::new("t").with_document_ids(vec!["a".into()]), docs.clone()); let plan = PushPlan::new(vec![RemoteId::new("a")], vec![PushOperation::AppendBlock { parent_id: RemoteId::new("a"), after: None, content: "allowed".into() }, PushOperation::ArchiveEntity { entity_id: RemoteId::new("a") }]); let ids = vec![PushOperationId("p:0".into()), PushOperationId("p:1".into())]; let push = PushId("p".into()); let mount = MountId::new("m"); assert!(connector.apply(ApplyPlanRequest { push_id: &push, mount_id: &mount, plan: &plan, operation_ids: &ids, remote_preconditions: &[], local_root: None }).is_err()); let non_root = PushPlan::new(vec![], vec![PushOperation::CreateEntity { parent_id: RemoteId::new("a"), parent_kind: None, parent_workspace: false, title: "nested".into(), properties: BTreeMap::new(), body: String::new(), source_path: Default::default() }]); assert!(connector.apply(ApplyPlanRequest { push_id: &push, mount_id: &mount, plan: &non_root, operation_ids: &ids[..1], remote_preconditions: &[], local_root: None }).is_err()); assert_eq!(*docs.gets.lock().unwrap(), 0); assert_eq!(*docs.creates.lock().unwrap(), 0); assert_eq!(*docs.batches.lock().unwrap(), 0); }
}

impl std::fmt::Debug for GoogleDocsConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoogleDocsConfig")
            .field("access_token", &"<redacted>")
            .field("document_ids", &self.document_ids)
            .finish()
    }
}

impl GoogleDocsConfig {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            document_ids: Vec::new(),
        }
    }

    pub fn with_document_ids(mut self, document_ids: Vec<String>) -> Self {
        self.document_ids = document_ids;
        self
    }
}

#[derive(Clone)]
pub struct GoogleDocsConnector {
    config: GoogleDocsConfig,
    docs: Arc<dyn GoogleDocsApi>,
}

impl std::fmt::Debug for GoogleDocsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleDocsConnector")
            .field("document_ids", &self.config.document_ids)
            .field("access_token", &"<redacted>")
            .finish()
    }
}

impl GoogleDocsConnector {
    pub fn new(config: GoogleDocsConfig) -> Self {
        let api = Arc::new(HttpGoogleApiClient::new(config.access_token.clone()));
        Self::with_documents(config, api)
    }

    pub fn with_documents(config: GoogleDocsConfig, docs: Arc<dyn GoogleDocsApi>) -> Self {
        Self { config, docs }
    }

    pub fn config(&self) -> &GoogleDocsConfig {
        &self.config
    }

    fn require_selected(&self, remote_id: &RemoteId) -> LocalityResult<()> {
        if self.config.document_ids.iter().any(|id| id == remote_id.as_str()) {
            Ok(())
        } else {
            Err(LocalityError::Guardrail(format!(
                "google docs document `{}` is not selected for this mount",
                remote_id.as_str()
            )))
        }
    }

}

impl Connector for GoogleDocsConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind(GOOGLE_DOCS_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: true,
            supports_entity_body_updates: false,
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
            PushOperationKind::CreateEntity,
        ]
        .into_iter()
        .collect()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        project_selected_documents(self.docs.as_ref(), &request.mount_id, &self.config.document_ids)
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        match request.container {
            locality_connector::ChildContainer::Root => Ok(ListChildrenResult::complete(project_selected_documents(self.docs.as_ref(), &request.mount_id, &self.config.document_ids)?)),
            _ => Ok(ListChildrenResult::complete(Vec::new())),
        }
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        self.require_selected(&request.remote_id)?;
        let document = self.docs.get_document(request.remote_id.as_str())?;
        Ok(RemoteObservation::new(request.mount_id, RemoteId::new(document.document_id.clone()), EntityKind::Page, document.title.clone(), page_document_path(Path::new(&slugify_title(&document.title))))
            .with_raw_metadata_json(document_metadata_json(&document))
            .with_remote_version(RemoteVersion::new(document_remote_version(&document))))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        self.require_selected(&request.remote_id)?;
        let document = self.docs.get_document(request.remote_id.as_str())?;
        let raw = serde_json::to_vec(&document).map_err(|error| {
            LocalityError::Io(format!("google docs native encode failed: {error}"))
        })?;
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: "google_docs_document".to_string(),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        let document = serde_json::from_slice::<GoogleDocument>(&entity.raw).map_err(|error| {
                LocalityError::Io(format!("google docs native decode failed: {error}"))
            })?;
        render_google_document(&document).map(|rendered| rendered.document)
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
        preflight_plan(request.plan, &self.config.document_ids)?;
        preflight_preconditions(request.remote_preconditions, &self.config.document_ids)?;
        check_remote_preconditions(self.docs.as_ref(), &request)
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        preflight_plan(request.plan, &self.config.document_ids)?;
        preflight_preconditions(request.remote_preconditions, &self.config.document_ids)?;
        apply_plan(self.docs.as_ref(), request)
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
            | PushOperation::MoveEntity { entity_id, .. }
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

fn document_metadata_json(document: &GoogleDocument) -> String {
    let mut value = serde_json::to_value(document).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(object) = &mut value {
        let search_metadata = document_search_metadata(document);
        if !search_metadata.is_empty()
            && let Ok(search_value) = serde_json::to_value(search_metadata)
        {
            object.insert(RAW_SEARCH_METADATA_KEY.to_string(), search_value);
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn document_search_metadata(document: &GoogleDocument) -> SearchMetadata {
    let mut metadata = SearchMetadata::default();
    metadata.push_metadata_text(&document.document_id);
    metadata.push_metadata_text(&document.title);
    metadata.push_metadata_text("Google Docs");
    metadata.push_alias(&document.document_id);
    metadata.set_source_url(format!("https://docs.google.com/document/d/{}/edit", document.document_id));
    if let Some(revision) = &document.revision_id { metadata.push_metadata_text(revision); }
    metadata
}

fn check_remote_preconditions(
    docs: &dyn GoogleDocsApi,
    request: &ApplyPlanRequest<'_>,
) -> LocalityResult<()> {
    for precondition in request.remote_preconditions {
        let Some(expected) = &precondition.remote_edited_at else {
            continue;
        };
        let current = remote_version_from_apis(docs, &precondition.remote_id)?;
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

fn remote_version_from_apis(
    docs: &dyn GoogleDocsApi,
    remote_id: &RemoteId,
) -> LocalityResult<String> {
    Ok(document_remote_version(&docs.get_document(remote_id.as_str())?))
}

fn project_selected_documents(docs: &dyn GoogleDocsApi, mount_id: &locality_core::model::MountId, document_ids: &[String]) -> LocalityResult<Vec<TreeEntry>> {
    let mut used_paths = BTreeSet::new();
    let mut documents = document_ids.iter().map(|id| docs.get_document(id)).collect::<LocalityResult<Vec<_>>>()?;
    documents.sort_by(|left, right| left.title.to_lowercase().cmp(&right.title.to_lowercase()).then_with(|| left.document_id.cmp(&right.document_id)));
    Ok(documents.into_iter().map(|document| TreeEntry {
                mount_id: mount_id.clone(),
                remote_id: RemoteId::new(document.document_id.clone()),
                kind: EntityKind::Page,
                title: document.title.clone(),
                path: allocate_path(Path::new(""), &document.title, &document.document_id, true, &mut used_paths),
                hydration: HydrationState::Stub,
                content_hash: None,
                remote_edited_at: Some(document_remote_version(&document)),
                stub_frontmatter: Some(document_frontmatter(&document)),
            }).collect())
}

/// Reject an entire plan before checking remote versions or issuing any Docs call.
fn preflight_plan(plan: &PushPlan, selected_ids: &[String]) -> LocalityResult<()> {
    let selected = |id: &str| selected_ids.iter().any(|selected_id| selected_id == id);
    for entity_id in &plan.affected_entities {
        if !selected(entity_id.as_str()) {
            return Err(LocalityError::Guardrail(format!("google docs document `{}` is not selected for this mount", entity_id.as_str())));
        }
    }
    for operation in &plan.operations {
        match operation {
            PushOperation::UpdateBlock { block_id, .. }
            | PushOperation::ReplaceBlock { block_id, .. }
            | PushOperation::ArchiveBlock { block_id } => {
                let range = GoogleBlockRange::parse(block_id)?;
                if !selected(&range.document_id) { return Err(LocalityError::Guardrail(format!("google docs document `{}` is not selected for this mount", range.document_id))); }
            }
            PushOperation::AppendBlock { parent_id, after, .. } => {
                if !selected(parent_id.as_str()) { return Err(LocalityError::Guardrail(format!("google docs document `{}` is not selected for this mount", parent_id.as_str()))); }
                if let Some(after) = after { let range = GoogleBlockRange::parse(after)?; if range.document_id != parent_id.0 { return Err(LocalityError::Guardrail("google docs append block belongs to a different document".to_string())); } }
            }
            PushOperation::CreateEntity { parent_id, parent_workspace, .. } if parent_id.as_str() == "root" && !parent_workspace => {}
            PushOperation::CreateEntity { .. } => return Err(LocalityError::Unsupported("google docs connector can only create documents at the mount root")),
            PushOperation::UpdateEntityBody { .. } => return Err(LocalityError::Unsupported("whole-entity body updates for Google Docs")),
            _ => return Err(LocalityError::Unsupported("Google Docs mounts only support body edits and root document creation")),
        }
    }
    Ok(())
}

fn preflight_preconditions(preconditions: &[locality_core::push::RemotePrecondition], selected_ids: &[String]) -> LocalityResult<()> {
    for precondition in preconditions {
        if !selected_ids.iter().any(|id| id == precondition.remote_id.as_str()) {
            return Err(LocalityError::Guardrail(format!("google docs document `{}` is not selected for this mount", precondition.remote_id.as_str())));
        }
    }
    Ok(())
}

fn apply_plan(
    docs: &dyn GoogleDocsApi,
    request: ApplyPlanRequest<'_>,
) -> LocalityResult<ApplyPlanResult> {
    check_remote_preconditions(docs, &request)?;
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
                let after_range = after
                    .as_ref()
                    .and_then(|after| GoogleBlockRange::parse(after).ok())
                    .map(|range| {
                        range.shifted_for_insertions(
                            inserted_ranges
                                .get(&range.document_id)
                                .map(Vec::as_slice)
                                .unwrap_or(&[]),
                        )
                    });
                let final_block_append = after_range
                    .as_ref()
                    .is_some_and(|range| range.end_index == document_end_index(&document));
                let base_index = after_range
                    .as_ref()
                    .map(|range| {
                        if final_block_append && range.end_index > range.start_index {
                            range.end_index - 1
                        } else {
                            range.end_index
                        }
                    })
                    .unwrap_or_else(|| document_start_index(&document));
                let append_key = (
                    parent_id.0.clone(),
                    after.as_ref().map(|remote_id| remote_id.0.clone()),
                );
                let index_position =
                    base_index + append_offsets.get(&append_key).copied().unwrap_or_default();
                let mut docs_text = docs_block_text(content);
                if final_block_append {
                    move_docs_text_before_segment_newline(&mut docs_text);
                }
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
                        parent_id.0,
                        if final_block_append {
                            index_position + 1
                        } else {
                            index_position
                        },
                        new_block_end
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
            PushOperation::UpdateEntityBody { .. } => {
                return Err(LocalityError::Unsupported(
                    "whole-entity body updates for Google Docs",
                ));
            }
            PushOperation::ArchiveEntity { .. }
            | PushOperation::UpdateProperties { .. }
            | PushOperation::MoveEntity { .. } => return Err(LocalityError::Unsupported("Google Docs mounts only support body edits; rename, move, and archive are unavailable")),
            PushOperation::CreateEntity {
                parent_id,
                parent_workspace,
                title,
                body,
                ..
            } => {
                if *parent_workspace || parent_id.0 != "root" {
                    return Err(LocalityError::Unsupported(
                        "google docs connector can only create documents at the mount root",
                    ));
                }
                let created = docs.create_document(title)?;
                if !body.trim().is_empty() {
                    if let Err(error) = docs.batch_update_document(
                        created.document_id.as_str(),
                        BatchUpdateDocumentRequest {
                            requests: docs_document_text_requests(1, body),
                            write_control: write_control(&created),
                        },
                    ) {
                        return Err(error);
                    }
                }
                let entity_id = RemoteId::new(created.document_id);
                changed.insert(entity_id.clone());
                effects.push(JournalApplyEffect::CreatedEntity {
                    operation_id,
                    operation_index: index,
                    parent_id: parent_id.clone(),
                    entity_id,
                });
            }
            PushOperation::MoveBlock { .. }
            | PushOperation::UpdateMedia { .. }
            | PushOperation::CreateDatabase { .. } => {
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
    indent_end: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocsParagraphSpacingRange {
    start: usize,
    end: usize,
    line_spacing: Option<serde_json::Value>,
    space_above: Option<serde_json::Value>,
    space_below: Option<serde_json::Value>,
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
    let preserved_paragraph_spacing = style_source
        .map(|source| preserved_paragraph_spacing(&inserted_text, source))
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
        preserved_paragraph_spacing
            .into_iter()
            .map(|range| paragraph_spacing_request(location_index, range)),
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
    if range.indent_end.is_some() {
        fields.push("indentEnd");
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
                indent_end: range.indent_end,
                ..ParagraphStylePatch::default()
            },
            fields: fields.join(","),
        },
    }
}

fn paragraph_spacing_request(
    location_index: usize,
    range: DocsParagraphSpacingRange,
) -> DocsRequest {
    let mut fields = Vec::new();
    if range.line_spacing.is_some() {
        fields.push("lineSpacing");
    }
    if range.space_above.is_some() {
        fields.push("spaceAbove");
    }
    if range.space_below.is_some() {
        fields.push("spaceBelow");
    }
    DocsRequest::UpdateParagraphStyle {
        update_paragraph_style: UpdateParagraphStyleRequest {
            range: Range {
                start_index: location_index + range.start,
                end_index: location_index + range.end,
            },
            paragraph_style: ParagraphStylePatch {
                line_spacing: range.line_spacing,
                space_above: range.space_above,
                space_below: range.space_below,
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
                indent_end: range.indent_end,
            })
        })
        .collect()
}

fn preserved_paragraph_spacing(
    new_text: &str,
    source: DocsTextStyleSource<'_>,
) -> Vec<DocsParagraphSpacingRange> {
    let (source_text, source_ranges) = source_text_paragraph_spacing_ranges(source);
    source_ranges
        .into_iter()
        .filter_map(|range| {
            let (start, end) =
                map_paragraph_range_by_context(&source_text, range.start, range.end, new_text)?;
            Some(DocsParagraphSpacingRange {
                start,
                end,
                line_spacing: range.line_spacing,
                space_above: range.space_above,
                space_below: range.space_below,
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
        if style.indent_start.is_none()
            && style.indent_first_line.is_none()
            && style.indent_end.is_none()
        {
            continue;
        }
        ranges.push(DocsParagraphIndentRange {
            start: range_start,
            end: range_end,
            indent_start: style.indent_start.clone(),
            indent_first_line: style.indent_first_line.clone(),
            indent_end: style.indent_end.clone(),
        });
    }
    (source_text, ranges)
}

fn source_text_paragraph_spacing_ranges(
    source: DocsTextStyleSource<'_>,
) -> (String, Vec<DocsParagraphSpacingRange>) {
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
        if style.line_spacing.is_none()
            && style.space_above.is_none()
            && style.space_below.is_none()
        {
            continue;
        }
        ranges.push(DocsParagraphSpacingRange {
            start: range_start,
            end: range_end,
            line_spacing: style.line_spacing.clone(),
            space_above: style.space_above.clone(),
            space_below: style.space_below.clone(),
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

fn move_docs_text_before_segment_newline(docs_text: &mut DocsText) {
    strip_trailing_segment_newline(docs_text);
    docs_text.text.insert(0, '\n');
    shift_docs_text_ranges(docs_text, 1);
}

fn shift_docs_text_ranges(docs_text: &mut DocsText, amount: usize) {
    for range in &mut docs_text.style_ranges {
        range.start += amount;
        range.end += amount;
    }
    for range in &mut docs_text.paragraph_styles {
        range.start += amount;
        range.end += amount;
    }
    for range in &mut docs_text.bullet_ranges {
        range.start += amount;
        range.end += amount;
    }
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
