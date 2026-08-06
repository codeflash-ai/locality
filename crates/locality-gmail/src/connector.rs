use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, ChildContainer,
    Connector, ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest,
    ListChildrenRequest, ListChildrenResult, NativeEntity, ObserveRequest, ParsedEntity,
};
use locality_core::freshness::{RemoteObservation, RemoteVersion};
use locality_core::journal::{JournalApplyEffect, PushId, PushOperationId};
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::{PropertyValue, PushOperation, PushOperationKind};
use locality_core::search::{RAW_SEARCH_METADATA_KEY, SearchMetadata};
use locality_core::validation::ValidationIssue;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::client::{GmailApi, HttpGmailApiClient};
use crate::dto::{
    GmailDraft, GmailDraftCreateRequest, GmailDraftSendRequest, GmailDraftUpdateRequest,
    GmailMessage, GmailMessageSendRequest, GmailRawMessage, GmailThread, header_map,
};
use crate::oauth::GMAIL_CONNECTOR_ID;
use crate::render::{
    GmailDraftDocument, GmailNativeBundle, GmailThreadMessageNativeBundle, GmailThreadNativeBundle,
    build_draft_mime_with_message_id, draft_remote_id, message_frontmatter,
    message_frontmatter_with_entity_id, parse_draft_remote_id, parse_thread_message_remote_id,
    parse_thread_remote_id, raw_message_base64url, remote_version, render_gmail_message,
    render_gmail_thread, render_gmail_thread_message, thread_message_remote_id, thread_remote_id,
    thread_remote_version,
};
use crate::settings::{GmailMountSettings, GmailProjectionView};

const GMAIL_PAGE_SIZE: u32 = 100;
const INBOX_FOLDER_ID: &str = "gmail-folder:inbox";
const SENT_FOLDER_ID: &str = "gmail-folder:sent";
const DRAFT_FOLDER_ID: &str = "gmail-folder:draft";
const OUTBOX_FOLDER_ID: &str = "gmail-folder:outbox";

#[derive(Clone, PartialEq, Eq)]
pub struct GmailConfig {
    pub access_token: String,
    pub settings: GmailMountSettings,
}

impl GmailConfig {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            settings: GmailMountSettings::default(),
        }
    }

    pub fn with_settings(mut self, settings: GmailMountSettings) -> Self {
        self.settings = settings;
        self
    }
}

impl fmt::Debug for GmailConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GmailConfig")
            .field("access_token", &"<redacted>")
            .field("settings", &self.settings)
            .finish()
    }
}

#[derive(Clone)]
pub struct GmailConnector {
    config: GmailConfig,
    api: Arc<dyn GmailApi>,
}

impl fmt::Debug for GmailConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GmailConnector")
            .field("access_token", &"<redacted>")
            .finish()
    }
}

impl GmailConnector {
    pub fn new(config: GmailConfig) -> Self {
        let api = Arc::new(HttpGmailApiClient::new(config.access_token.clone()));
        Self::with_api(config, api)
    }

    pub fn with_api(config: GmailConfig, api: Arc<dyn GmailApi>) -> Self {
        Self { config, api }
    }

    pub fn config(&self) -> &GmailConfig {
        &self.config
    }

    pub fn api(&self) -> &dyn GmailApi {
        self.api.as_ref()
    }
}

impl Connector for GmailConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind(GMAIL_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: false,
            supports_entity_body_updates: true,
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
            PushOperationKind::CreateEntity,
            PushOperationKind::UpdateProperties,
            PushOperationKind::UpdateEntityBody,
            PushOperationKind::MoveEntity,
        ]
        .into_iter()
        .collect()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        if self.config.settings.gmail.view == GmailProjectionView::Threads {
            let mut entries = gmail_folder_entries(&request.mount_id, Path::new(""));
            entries.extend(list_thread_entries(
                self.api.as_ref(),
                &self.config.settings,
                &request.mount_id,
                "INBOX",
                "inbox",
                Path::new("inbox"),
            )?);
            entries.extend(list_thread_entries(
                self.api.as_ref(),
                &self.config.settings,
                &request.mount_id,
                "SENT",
                "sent",
                Path::new("sent"),
            )?);
            entries.extend(list_draft_entries(
                self.api.as_ref(),
                &self.config.settings,
                &request.mount_id,
                Path::new("draft"),
            )?);
            return Ok(entries);
        }

        let mut entries = gmail_folder_entries(&request.mount_id, Path::new(""));
        entries.extend(list_label_entries(
            self.api.as_ref(),
            &self.config.settings,
            &request.mount_id,
            "INBOX",
            "inbox",
            Path::new("inbox"),
        )?);
        entries.extend(list_label_entries(
            self.api.as_ref(),
            &self.config.settings,
            &request.mount_id,
            "SENT",
            "sent",
            Path::new("sent"),
        )?);
        entries.extend(list_draft_entries(
            self.api.as_ref(),
            &self.config.settings,
            &request.mount_id,
            Path::new("draft"),
        )?);
        Ok(entries)
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let entries = match request.container {
            ChildContainer::Root => gmail_folder_entries(&request.mount_id, &request.parent_path),
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == INBOX_FOLDER_ID =>
            {
                if self.config.settings.gmail.view == GmailProjectionView::Threads {
                    list_thread_entries(
                        self.api.as_ref(),
                        &self.config.settings,
                        &request.mount_id,
                        "INBOX",
                        "inbox",
                        &request.parent_path,
                    )?
                } else {
                    list_label_entries(
                        self.api.as_ref(),
                        &self.config.settings,
                        &request.mount_id,
                        "INBOX",
                        "inbox",
                        &request.parent_path,
                    )?
                }
            }
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == SENT_FOLDER_ID =>
            {
                if self.config.settings.gmail.view == GmailProjectionView::Threads {
                    list_thread_entries(
                        self.api.as_ref(),
                        &self.config.settings,
                        &request.mount_id,
                        "SENT",
                        "sent",
                        &request.parent_path,
                    )?
                } else {
                    list_label_entries(
                        self.api.as_ref(),
                        &self.config.settings,
                        &request.mount_id,
                        "SENT",
                        "sent",
                        &request.parent_path,
                    )?
                }
            }
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == DRAFT_FOLDER_ID =>
            {
                list_draft_entries(
                    self.api.as_ref(),
                    &self.config.settings,
                    &request.mount_id,
                    &request.parent_path,
                )?
            }
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == OUTBOX_FOLDER_ID =>
            {
                Vec::new()
            }
            ChildContainer::PageChildren(remote_id) => {
                let Some((mailbox, thread_id)) = parse_thread_remote_id(&remote_id) else {
                    return Ok(ListChildrenResult::complete(Vec::new()));
                };
                let thread = self.api.get_thread_metadata(thread_id)?;
                thread
                    .messages
                    .into_iter()
                    .map(|message| {
                        Ok(thread_message_entry(
                            &request.mount_id,
                            &request.parent_path,
                            mailbox,
                            thread_id,
                            message,
                        ))
                    })
                    .collect::<LocalityResult<Vec<_>>>()?
            }
            _ => Vec::new(),
        };
        Ok(ListChildrenResult::complete(entries))
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        if let Some(folder) = folder_spec(request.remote_id.as_str()) {
            return Ok(folder_observation(
                request.mount_id,
                &request.remote_id,
                folder,
            ));
        }

        if let Some((mailbox, thread_id, message_id)) =
            parse_thread_message_remote_id(&request.remote_id)
        {
            let mailbox = mailbox.to_string();
            let thread_id = thread_id.to_string();
            let message_id = message_id.to_string();
            let thread = self.api.get_thread_metadata(&thread_id)?;
            let thread_title = thread
                .messages
                .first()
                .map(message_subject)
                .unwrap_or_else(|| "(no subject)".to_string());
            let thread_path =
                Path::new(&mailbox).join(thread_directory_name(&thread, &thread_title));
            let message = thread
                .messages
                .iter()
                .find(|message| message.id == message_id)
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| self.api.get_message_metadata(&message_id))?;
            let entry = thread_message_entry(
                &request.mount_id,
                &thread_path,
                &mailbox,
                &thread_id,
                message.clone(),
            );
            return Ok(RemoteObservation::new(
                request.mount_id,
                request.remote_id,
                EntityKind::Page,
                entry.title,
                entry.path,
            )
            .with_parent(thread_remote_id(&mailbox, &thread_id))
            .with_remote_version(RemoteVersion::new(remote_version(&message)))
            .with_raw_metadata_json(gmail_message_metadata_json(
                &message,
                &mailbox,
                Some(&thread_id),
            )));
        }

        if let Some((mailbox, thread_id)) = parse_thread_remote_id(&request.remote_id) {
            let mailbox = mailbox.to_string();
            let thread_id = thread_id.to_string();
            let thread = self.api.get_thread_metadata(&thread_id)?;
            let entry = thread_entry(
                &request.mount_id,
                Path::new(&mailbox),
                &mailbox,
                thread.clone(),
            );
            return Ok(RemoteObservation::new(
                request.mount_id,
                request.remote_id,
                EntityKind::Page,
                entry.title,
                entry.path,
            )
            .with_parent(RemoteId::new(mailbox_folder_id(&mailbox)))
            .with_remote_version(RemoteVersion::new(thread_remote_version(&thread)))
            .with_raw_metadata_json(gmail_thread_metadata_json(&thread, &mailbox)));
        }

        if let Some(draft_id) = parse_draft_remote_id(&request.remote_id) {
            let draft = self.api.get_draft_full(draft_id)?;
            let entry = draft_entry(
                &request.mount_id,
                Path::new("draft"),
                draft.id.clone(),
                draft.message.clone(),
            )?;
            return Ok(RemoteObservation::new(
                request.mount_id,
                draft_remote_id(&draft.id),
                EntityKind::Page,
                entry.title,
                entry.path,
            )
            .with_parent(RemoteId::new(DRAFT_FOLDER_ID))
            .with_remote_version(RemoteVersion::new(remote_version(&draft.message)))
            .with_raw_metadata_json(gmail_message_metadata_json(&draft.message, "draft", None)));
        }

        let message = self.api.get_message_metadata(request.remote_id.as_str())?;
        let mailbox = mailbox_from_labels(&message.label_ids);
        let parent_id = mailbox_folder_id(mailbox);
        let entry = message_entry(
            &request.mount_id,
            Path::new(mailbox),
            mailbox,
            message.clone(),
        );
        Ok(RemoteObservation::new(
            request.mount_id,
            RemoteId::new(message.id.clone()),
            EntityKind::Page,
            entry.title,
            entry.path,
        )
        .with_parent(RemoteId::new(parent_id))
        .with_remote_version(RemoteVersion::new(remote_version(&message)))
        .with_raw_metadata_json(gmail_message_metadata_json(&message, mailbox, None)))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        if let Some((mailbox, thread_id, message_id)) =
            parse_thread_message_remote_id(&request.remote_id)
        {
            let message = self.api.get_message_full(message_id)?;
            let bundle = GmailThreadMessageNativeBundle {
                mailbox: mailbox.to_string(),
                thread_id: thread_id.to_string(),
                message,
            };
            let raw = serde_json::to_vec(&bundle).map_err(|error| {
                LocalityError::Io(format!(
                    "gmail thread message native encode failed: {error}"
                ))
            })?;
            return Ok(NativeEntity {
                remote_id: request.remote_id,
                kind: "gmail_thread_message".to_string(),
                raw,
            });
        }

        if let Some((mailbox, thread_id)) = parse_thread_remote_id(&request.remote_id) {
            let mailbox = mailbox.to_string();
            let thread_id = thread_id.to_string();
            let thread = self.api.get_thread_full(&thread_id)?;
            let bundle = GmailThreadNativeBundle { mailbox, thread };
            let raw = serde_json::to_vec(&bundle).map_err(|error| {
                LocalityError::Io(format!("gmail thread native encode failed: {error}"))
            })?;
            return Ok(NativeEntity {
                remote_id: request.remote_id,
                kind: "gmail_thread".to_string(),
                raw,
            });
        }

        if let Some(draft_id) = parse_draft_remote_id(&request.remote_id) {
            let draft = self.api.get_draft_full(draft_id)?;
            let remote_id = draft_remote_id(&draft.id);
            let bundle = GmailNativeBundle {
                mailbox: "draft".to_string(),
                draft_id: Some(draft.id),
                message: draft.message,
            };
            let raw = serde_json::to_vec(&bundle).map_err(|error| {
                LocalityError::Io(format!("gmail draft native encode failed: {error}"))
            })?;
            return Ok(NativeEntity {
                remote_id,
                kind: "gmail_message".to_string(),
                raw,
            });
        }

        let message = self.api.get_message_full(request.remote_id.as_str())?;
        let bundle = GmailNativeBundle {
            mailbox: mailbox_from_labels(&message.label_ids).to_string(),
            draft_id: None,
            message,
        };
        let raw = serde_json::to_vec(&bundle)
            .map_err(|error| LocalityError::Io(format!("gmail native encode failed: {error}")))?;
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: "gmail_message".to_string(),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        if entity.kind == "gmail_thread" {
            let bundle = serde_json::from_slice::<GmailThreadNativeBundle>(&entity.raw).map_err(
                |error| LocalityError::Io(format!("gmail thread native decode failed: {error}")),
            )?;
            return render_gmail_thread(&bundle).map(|rendered| rendered.document);
        }

        if entity.kind == "gmail_thread_message" {
            let bundle = serde_json::from_slice::<GmailThreadMessageNativeBundle>(&entity.raw)
                .map_err(|error| {
                    LocalityError::Io(format!(
                        "gmail thread message native decode failed: {error}"
                    ))
                })?;
            return render_gmail_thread_message(&bundle).map(|rendered| rendered.document);
        }

        let bundle = serde_json::from_slice::<GmailNativeBundle>(&entity.raw)
            .map_err(|error| LocalityError::Io(format!("gmail native decode failed: {error}")))?;
        render_gmail_message(&bundle).map(|rendered| rendered.document)
    }

    fn parse(&self, document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        let draft = parse_gmail_draft_document(document)?;
        let raw = serde_json::to_vec(&DraftNative::from(draft))
            .map_err(|error| LocalityError::Io(format!("gmail draft encode failed: {error}")))?;
        Ok(ParsedEntity {
            remote_id: RemoteId::new("gmail-draft:local"),
            native: NativeEntity {
                remote_id: RemoteId::new("gmail-draft:local"),
                kind: "gmail_draft".to_string(),
                raw,
            },
        })
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Ok(())
    }

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        let mut changed_remote_ids = Vec::new();
        let mut effects = Vec::new();
        let mut draft_mutations = BTreeMap::<String, DraftApplyMutation>::new();

        for (index, operation) in request.plan.operations.iter().enumerate() {
            let operation_id =
                request.operation_ids.get(index).cloned().ok_or_else(|| {
                    LocalityError::InvalidState("missing operation id".to_string())
                })?;

            match operation {
                PushOperation::CreateEntity {
                    parent_id,
                    parent_kind,
                    parent_workspace,
                    title,
                    properties,
                    body,
                    source_path,
                } => {
                    let outbound_target = outbound_target_from_create(
                        parent_id,
                        parent_kind,
                        *parent_workspace,
                        source_path,
                    )?;

                    let message_id = locality_message_id(request.push_id, &operation_id);
                    if let Some(sent) =
                        find_sent_message_by_message_id(self.api.as_ref(), &message_id)?
                    {
                        let sent_id = RemoteId::new(sent.id);
                        changed_remote_ids.push(sent_id.clone());
                        effects.push(JournalApplyEffect::CreatedEntity {
                            operation_id,
                            operation_index: index,
                            parent_id: RemoteId::new(SENT_FOLDER_ID),
                            entity_id: sent_id,
                        });
                        continue;
                    }

                    let draft = draft_from_push_create(title, properties, body)?;
                    let mime = build_draft_mime_with_message_id(&draft, Some(&message_id))?;
                    let raw = raw_message_base64url(&mime);
                    match outbound_target {
                        OutboundTarget::Draft => {
                            let created = self.api.create_draft(GmailDraftCreateRequest {
                                message: GmailRawMessage { raw },
                            })?;
                            let created_draft_id = draft_remote_id(&created.id);
                            changed_remote_ids.push(created_draft_id.clone());
                            effects.push(JournalApplyEffect::CreatedEntity {
                                operation_id,
                                operation_index: index,
                                parent_id: RemoteId::new(DRAFT_FOLDER_ID),
                                entity_id: created_draft_id,
                            });
                        }
                        OutboundTarget::Send => {
                            let sent = match self.api.send_message(GmailMessageSendRequest { raw })
                            {
                                Ok(sent) => sent,
                                Err(send_error) => {
                                    match find_sent_message_by_message_id(
                                        self.api.as_ref(),
                                        &message_id,
                                    ) {
                                        Ok(Some(sent)) => sent,
                                        Ok(None) => return Err(send_error),
                                        Err(lookup_error) => {
                                            return Err(LocalityError::Io(format!(
                                                "gmail send ambiguous after send failure; sent lookup failed: {lookup_error}"
                                            )));
                                        }
                                    }
                                }
                            };
                            let sent_id = RemoteId::new(sent.id);
                            changed_remote_ids.push(sent_id.clone());
                            effects.push(JournalApplyEffect::CreatedEntity {
                                operation_id,
                                operation_index: index,
                                parent_id: RemoteId::new(SENT_FOLDER_ID),
                                entity_id: sent_id,
                            });
                        }
                    }
                }
                PushOperation::UpdateProperties {
                    entity_id,
                    properties,
                } => {
                    let Some(draft_id) = parse_draft_remote_id(entity_id).map(str::to_string)
                    else {
                        return Err(LocalityError::Unsupported("gmail push operation"));
                    };
                    let mutation = draft_mutation(
                        &mut draft_mutations,
                        entity_id,
                        &draft_id,
                        index,
                        operation_id,
                    );
                    mutation.properties.extend(properties.clone());
                }
                PushOperation::UpdateEntityBody { entity_id, body } => {
                    let Some(draft_id) = parse_draft_remote_id(entity_id).map(str::to_string)
                    else {
                        return Err(LocalityError::Unsupported("gmail push operation"));
                    };
                    let mutation = draft_mutation(
                        &mut draft_mutations,
                        entity_id,
                        &draft_id,
                        index,
                        operation_id,
                    );
                    mutation.body = Some(body.clone());
                }
                PushOperation::MoveEntity {
                    entity_id,
                    new_parent_id,
                    new_title,
                    ..
                } => {
                    let Some(draft_id) = parse_draft_remote_id(entity_id).map(str::to_string)
                    else {
                        return Err(LocalityError::Unsupported("gmail push operation"));
                    };
                    if new_parent_id.as_str() != OUTBOX_FOLDER_ID {
                        return Err(LocalityError::Unsupported("gmail draft move parent"));
                    }
                    let mutation = draft_mutation(
                        &mut draft_mutations,
                        entity_id,
                        &draft_id,
                        index,
                        operation_id.clone(),
                    );
                    mutation.move_to_outbox = true;
                    mutation.title = Some(new_title.clone());
                    mutation.operation_index = index;
                    mutation.operation_id = Some(operation_id);
                }
                _ => return Err(LocalityError::Unsupported("gmail push operation")),
            }
        }

        for mutation in draft_mutations.into_values() {
            let current = self.api.get_draft_full(&mutation.draft_id)?;
            let mut draft_seed = draft_document_from_remote_draft(&current)?;
            apply_draft_mutation(
                &mut draft_seed.document,
                &mutation,
                Some(&draft_seed.baseline_title),
            )?;
            update_gmail_draft_from_document(
                self.api.as_ref(),
                &mutation.draft_id,
                &draft_seed.document,
            )?;

            if mutation.move_to_outbox {
                let sent = self
                    .api
                    .send_draft(GmailDraftSendRequest {
                        id: mutation.draft_id.clone(),
                    })
                    .map_err(|error| {
                        LocalityError::Io(format!(
                            "gmail draft send ambiguous after draft update: {error}"
                        ))
                    })?;
                let sent_id = RemoteId::new(sent.id);
                changed_remote_ids.push(sent_id.clone());
                let operation_id = mutation.operation_id.clone().ok_or_else(|| {
                    LocalityError::InvalidState("missing gmail draft send operation id".to_string())
                })?;
                effects.push(JournalApplyEffect::ArchivedEntity {
                    operation_id: operation_id.clone(),
                    operation_index: mutation.operation_index,
                    entity_id: mutation.draft_remote_id.clone(),
                });
                effects.push(JournalApplyEffect::CreatedEntity {
                    operation_id,
                    operation_index: mutation.operation_index,
                    parent_id: RemoteId::new(SENT_FOLDER_ID),
                    entity_id: sent_id,
                });
            } else {
                changed_remote_ids.push(mutation.draft_remote_id);
            }
        }

        Ok(ApplyPlanResult {
            changed_remote_ids,
            effects,
        })
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::Unsupported("gmail undo"))
    }
}

fn find_sent_message_by_message_id(
    api: &dyn GmailApi,
    message_id: &str,
) -> LocalityResult<Option<GmailMessage>> {
    let query = format!("rfc822msgid:<{message_id}>");
    let list = api.list_messages("SENT", 10, None, Some(&query))?;
    let Some(message_ref) = list.messages.first() else {
        return Ok(None);
    };

    api.get_message_metadata(&message_ref.id).map(Some)
}

fn locality_message_id(push_id: &PushId, operation_id: &PushOperationId) -> String {
    let seed = format!("{}:{}", push_id.0, operation_id.0);
    let mut encoded = String::with_capacity(seed.len() * 2);
    for byte in seed.as_bytes() {
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    format!("loc-{encoded}@locality.local")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FolderSpec {
    id: &'static str,
    title: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutboundTarget {
    Draft,
    Send,
}

#[derive(Clone, Debug)]
struct DraftApplyMutation {
    draft_remote_id: RemoteId,
    draft_id: String,
    move_to_outbox: bool,
    title: Option<String>,
    properties: BTreeMap<String, PropertyValue>,
    body: Option<String>,
    operation_index: usize,
    operation_id: Option<PushOperationId>,
}

fn draft_mutation<'a>(
    mutations: &'a mut BTreeMap<String, DraftApplyMutation>,
    draft_remote_id: &RemoteId,
    draft_id: &str,
    operation_index: usize,
    operation_id: PushOperationId,
) -> &'a mut DraftApplyMutation {
    mutations
        .entry(draft_id.to_string())
        .or_insert_with(|| DraftApplyMutation {
            draft_remote_id: draft_remote_id.clone(),
            draft_id: draft_id.to_string(),
            move_to_outbox: false,
            title: None,
            properties: BTreeMap::new(),
            body: None,
            operation_index,
            operation_id: Some(operation_id),
        })
}

struct DraftApplySeed {
    document: GmailDraftDocument,
    baseline_title: String,
}

fn draft_document_from_remote_draft(draft: &GmailDraft) -> LocalityResult<DraftApplySeed> {
    let bundle = GmailNativeBundle {
        mailbox: "draft".to_string(),
        draft_id: Some(draft.id.clone()),
        message: draft.message.clone(),
    };
    let rendered = render_gmail_message(&bundle)?;
    if !rendered.attachment_specs.is_empty() {
        return Err(LocalityError::Unsupported("gmail attachments"));
    }
    let remote_id = draft_remote_id(&draft.id);
    let document = CanonicalDocument::new(
        message_frontmatter_with_entity_id(&bundle, &remote_id),
        rendered.document.body,
    );
    let draft = parse_gmail_draft_document(&document)?;
    let baseline_title = draft.subject.clone();
    Ok(DraftApplySeed {
        document: draft,
        baseline_title,
    })
}

fn apply_draft_mutation(
    draft: &mut GmailDraftDocument,
    mutation: &DraftApplyMutation,
    baseline_title: Option<&str>,
) -> LocalityResult<()> {
    if draft_properties_have_attachments(&mutation.properties) {
        return Err(LocalityError::Unsupported("gmail attachments"));
    }
    if mutation.properties.contains_key("to") {
        draft.to = recipients_property(&mutation.properties, "to");
    }
    if mutation.properties.contains_key("cc") {
        draft.cc = recipients_property(&mutation.properties, "cc");
    }
    if mutation.properties.contains_key("bcc") {
        draft.bcc = recipients_property(&mutation.properties, "bcc");
    }
    if let Some(subject) = non_empty_string_property(&mutation.properties, "subject") {
        draft.subject = subject;
    } else if let Some(title) = non_empty_string_property(&mutation.properties, "title") {
        draft.subject = title;
    } else if let Some(title) = mutation.title.as_ref().filter(|title| {
        !title.trim().is_empty()
            && (draft.subject.trim().is_empty()
                || mutation.properties.contains_key("subject")
                || mutation.properties.contains_key("title"))
    }) {
        draft.subject = title.clone();
    } else if let Some(title) = baseline_title.filter(|title| !title.trim().is_empty()) {
        draft.subject = title.to_string();
    } else if mutation.properties.contains_key("subject")
        || mutation.properties.contains_key("title")
        || mutation.move_to_outbox
    {
        draft.subject.clear();
    }
    if let Some(body) = &mutation.body {
        draft.body = body.clone();
    }
    Ok(())
}

fn update_gmail_draft_from_document(
    api: &dyn GmailApi,
    draft_id: &str,
    draft: &GmailDraftDocument,
) -> LocalityResult<GmailDraft> {
    let raw = raw_message_base64url(&build_draft_mime_with_message_id(draft, None)?);
    api.update_draft(
        draft_id,
        GmailDraftUpdateRequest {
            message: GmailRawMessage { raw },
        },
    )
}

fn folder_specs() -> [FolderSpec; 4] {
    [
        FolderSpec {
            id: INBOX_FOLDER_ID,
            title: "inbox",
        },
        FolderSpec {
            id: SENT_FOLDER_ID,
            title: "sent",
        },
        FolderSpec {
            id: DRAFT_FOLDER_ID,
            title: "draft",
        },
        FolderSpec {
            id: OUTBOX_FOLDER_ID,
            title: "outbox",
        },
    ]
}

fn folder_spec(remote_id: &str) -> Option<FolderSpec> {
    folder_specs()
        .into_iter()
        .find(|folder| folder.id == remote_id)
}

fn gmail_folder_entries(mount_id: &MountId, parent_path: &Path) -> Vec<TreeEntry> {
    folder_specs()
        .into_iter()
        .map(|folder| TreeEntry {
            mount_id: mount_id.clone(),
            remote_id: RemoteId::new(folder.id),
            kind: EntityKind::Directory,
            title: folder.title.to_string(),
            path: parent_path.join(folder.title),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: Some(format!("folder:{}", folder.title)),
            stub_frontmatter: None,
        })
        .collect()
}

fn folder_observation(
    mount_id: MountId,
    remote_id: &RemoteId,
    folder: FolderSpec,
) -> RemoteObservation {
    RemoteObservation::new(
        mount_id,
        remote_id.clone(),
        EntityKind::Directory,
        folder.title,
        folder.title,
    )
    .with_remote_version(RemoteVersion::new(format!("folder:{}", folder.title)))
    .with_raw_metadata_json(gmail_folder_metadata_json(folder))
}

fn gmail_message_metadata_json(
    message: &GmailMessage,
    mailbox: &str,
    thread_id: Option<&str>,
) -> String {
    metadata_json(
        message,
        gmail_message_search_metadata(message, mailbox, thread_id),
    )
}

fn gmail_thread_metadata_json(thread: &GmailThread, mailbox: &str) -> String {
    metadata_json(thread, gmail_thread_search_metadata(thread, mailbox))
}

fn metadata_json<T>(value: &T, search_metadata: SearchMetadata) -> String
where
    T: Serialize,
{
    let mut value = serde_json::to_value(value).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(object) = &mut value
        && !search_metadata.is_empty()
        && let Ok(search_value) = serde_json::to_value(search_metadata)
    {
        object.insert(RAW_SEARCH_METADATA_KEY.to_string(), search_value);
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn gmail_folder_metadata_json(folder: FolderSpec) -> String {
    let mut search_metadata = SearchMetadata::default();
    search_metadata.push_metadata_text(folder.title);
    search_metadata.push_metadata_text(folder.id);
    search_metadata.push_alias(folder.id);
    let mut value = serde_json::json!({
        "kind": "gmail_folder",
        "id": folder.id,
        "title": folder.title,
    });
    if let serde_json::Value::Object(object) = &mut value
        && let Ok(search_value) = serde_json::to_value(search_metadata)
    {
        object.insert(RAW_SEARCH_METADATA_KEY.to_string(), search_value);
    }
    value.to_string()
}

fn gmail_message_search_metadata(
    message: &GmailMessage,
    mailbox: &str,
    thread_id: Option<&str>,
) -> SearchMetadata {
    let mut metadata = SearchMetadata::default();
    metadata.push_metadata_text(mailbox);
    push_gmail_message_search_values(&mut metadata, message);
    metadata.push_alias(&message.id);
    let source_thread_id = thread_id
        .map(str::to_string)
        .or_else(|| message.thread_id.clone())
        .unwrap_or_else(|| message.id.clone());
    metadata.push_alias(&source_thread_id);
    metadata.set_source_url(gmail_source_url(&source_thread_id));
    metadata
}

fn gmail_thread_search_metadata(thread: &GmailThread, mailbox: &str) -> SearchMetadata {
    let mut metadata = SearchMetadata::default();
    metadata.push_metadata_text(mailbox);
    metadata.push_metadata_text(&thread.id);
    metadata.push_alias(&thread.id);
    if let Some(history_id) = &thread.history_id {
        metadata.push_metadata_text(history_id);
    }
    for message in &thread.messages {
        push_gmail_message_search_values(&mut metadata, message);
        metadata.push_alias(&message.id);
    }
    metadata.set_source_url(gmail_source_url(&thread.id));
    metadata
}

fn push_gmail_message_search_values(metadata: &mut SearchMetadata, message: &GmailMessage) {
    metadata.push_metadata_text(&message.id);
    if let Some(thread_id) = &message.thread_id {
        metadata.push_metadata_text(thread_id);
    }
    for label in &message.label_ids {
        metadata.push_metadata_text(label);
    }
    if let Some(snippet) = &message.snippet {
        metadata.push_metadata_text(snippet);
    }
    if let Some(internal_date) = &message.internal_date {
        metadata.push_metadata_text(internal_date);
    }
    let headers = message.payload.as_ref().map(header_map).unwrap_or_default();
    for header in [
        "subject",
        "from",
        "to",
        "cc",
        "bcc",
        "reply-to",
        "sender",
        "date",
        "message-id",
        "list-id",
    ] {
        if let Some(value) = headers.get(header) {
            metadata.push_metadata_text(value);
        }
    }
}

fn gmail_source_url(id: &str) -> String {
    format!("https://mail.google.com/mail/u/0/#all/{id}")
}

fn list_label_entries(
    api: &dyn GmailApi,
    settings: &GmailMountSettings,
    mount_id: &MountId,
    label_id: &str,
    mailbox: &str,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let messages = list_message_refs(api, settings, label_id)?;
    messages
        .into_iter()
        .map(|message_ref| {
            let message = api.get_message_metadata(&message_ref.id)?;
            Ok(message_entry(mount_id, parent_path, mailbox, message))
        })
        .collect()
}

fn list_draft_entries(
    api: &dyn GmailApi,
    settings: &GmailMountSettings,
    mount_id: &MountId,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let Some(query) = gmail_recent_query(settings) else {
        let list = api.list_drafts(GMAIL_PAGE_SIZE, None, None)?;
        return draft_refs_to_entries(api, mount_id, parent_path, list.drafts);
    };

    let mut entries = Vec::new();
    let mut page_token: Option<String> = None;
    let mut seen_page_tokens = BTreeSet::new();
    loop {
        let list = api.list_drafts(GMAIL_PAGE_SIZE, page_token.as_deref(), Some(&query))?;
        for draft in list.drafts {
            entries.push(draft_ref_to_entry(api, mount_id, parent_path, draft)?);
        }
        let Some(next) = list.next_page_token else {
            break;
        };
        if !seen_page_tokens.insert(next.clone()) {
            return Err(LocalityError::InvalidState(format!(
                "gmail pagination returned repeated page token `{next}` for drafts"
            )));
        }
        page_token = Some(next);
    }
    Ok(entries)
}

fn draft_refs_to_entries(
    api: &dyn GmailApi,
    mount_id: &MountId,
    parent_path: &Path,
    drafts: Vec<crate::dto::GmailDraftRef>,
) -> LocalityResult<Vec<TreeEntry>> {
    drafts
        .into_iter()
        .map(|draft| draft_ref_to_entry(api, mount_id, parent_path, draft))
        .collect()
}

fn draft_ref_to_entry(
    api: &dyn GmailApi,
    mount_id: &MountId,
    parent_path: &Path,
    draft: crate::dto::GmailDraftRef,
) -> LocalityResult<TreeEntry> {
    let draft = api.get_draft_full(&draft.id)?;
    draft_entry(mount_id, parent_path, draft.id, draft.message)
}

fn list_thread_entries(
    api: &dyn GmailApi,
    settings: &GmailMountSettings,
    mount_id: &MountId,
    label_id: &str,
    mailbox: &str,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let threads = list_thread_refs(api, settings, label_id)?;
    let mut entries = Vec::new();
    for thread_ref in threads {
        let thread = api.get_thread_metadata(&thread_ref.id)?;
        if thread_starts_in_date_window(settings, &thread) {
            entries.push(thread_entry(mount_id, parent_path, mailbox, thread));
        }
    }
    Ok(entries)
}

fn list_message_refs(
    api: &dyn GmailApi,
    settings: &GmailMountSettings,
    label_id: &str,
) -> LocalityResult<Vec<crate::dto::GmailMessageRef>> {
    let Some(query) = settings
        .gmail
        .date_window
        .as_ref()
        .map(|window| window.query())
    else {
        return Ok(api
            .list_messages(label_id, GMAIL_PAGE_SIZE, None, None)?
            .messages);
    };

    let mut page_token = None;
    let mut seen_page_tokens = BTreeSet::new();
    let mut messages = Vec::new();
    loop {
        let page = api.list_messages(
            label_id,
            GMAIL_PAGE_SIZE,
            page_token.as_deref(),
            Some(&query),
        )?;
        messages.extend(page.messages);
        let Some(next) = page.next_page_token else {
            break;
        };
        if !seen_page_tokens.insert(next.clone()) {
            return Err(LocalityError::InvalidState(format!(
                "gmail pagination returned repeated page token `{next}` for label `{label_id}`"
            )));
        }
        page_token = Some(next);
    }
    Ok(messages)
}

fn gmail_recent_query(settings: &GmailMountSettings) -> Option<String> {
    settings
        .gmail
        .date_window
        .as_ref()
        .map(|window| window.query())
}

fn list_thread_refs(
    api: &dyn GmailApi,
    settings: &GmailMountSettings,
    label_id: &str,
) -> LocalityResult<Vec<crate::dto::GmailThreadRef>> {
    let Some(query) = settings
        .gmail
        .date_window
        .as_ref()
        .map(|window| window.query())
    else {
        return Ok(api
            .list_threads(label_id, GMAIL_PAGE_SIZE, None, None)?
            .threads);
    };

    let mut page_token = None;
    let mut seen_page_tokens = BTreeSet::new();
    let mut threads = Vec::new();
    loop {
        let page = api.list_threads(
            label_id,
            GMAIL_PAGE_SIZE,
            page_token.as_deref(),
            Some(&query),
        )?;
        threads.extend(page.threads);
        let Some(next) = page.next_page_token else {
            break;
        };
        if !seen_page_tokens.insert(next.clone()) {
            return Err(LocalityError::InvalidState(format!(
                "gmail pagination returned repeated page token `{next}` for thread label `{label_id}`"
            )));
        }
        page_token = Some(next);
    }
    Ok(threads)
}

fn message_entry(
    mount_id: &MountId,
    parent_path: &Path,
    mailbox: &str,
    message: GmailMessage,
) -> TreeEntry {
    let title = message_subject(&message);
    let version = remote_version(&message);
    let path = parent_path.join(message_filename(&message, &title));
    let bundle = GmailNativeBundle {
        mailbox: mailbox.to_string(),
        draft_id: None,
        message: message.clone(),
    };
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(message.id),
        kind: EntityKind::Page,
        title,
        path,
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(version),
        stub_frontmatter: Some(message_frontmatter(&bundle)),
    }
}

fn draft_entry(
    mount_id: &MountId,
    parent_path: &Path,
    draft_id: String,
    message: GmailMessage,
) -> LocalityResult<TreeEntry> {
    let title = message_subject(&message);
    let version = remote_version(&message);
    let path = parent_path.join(message_filename(&message, &title));
    let remote_id = draft_remote_id(&draft_id);
    let bundle = GmailNativeBundle {
        mailbox: "draft".to_string(),
        draft_id: Some(draft_id),
        message,
    };
    let stub_frontmatter = Some(message_frontmatter_with_entity_id(&bundle, &remote_id));
    Ok(TreeEntry {
        mount_id: mount_id.clone(),
        remote_id,
        kind: EntityKind::Page,
        title,
        path,
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(version),
        stub_frontmatter,
    })
}

fn thread_message_entry(
    mount_id: &MountId,
    parent_path: &Path,
    mailbox: &str,
    thread_id: &str,
    message: GmailMessage,
) -> TreeEntry {
    let title = message_subject(&message);
    let version = remote_version(&message);
    let path = parent_path.join(message_filename(&message, &title));
    let remote_id = thread_message_remote_id(mailbox, thread_id, &message.id);
    let bundle = GmailThreadMessageNativeBundle {
        mailbox: mailbox.to_string(),
        thread_id: thread_id.to_string(),
        message,
    };
    let stub_frontmatter = render_gmail_thread_message(&bundle)
        .ok()
        .map(|rendered| rendered.document.frontmatter);

    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id,
        kind: EntityKind::Page,
        title,
        path,
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(version),
        stub_frontmatter,
    }
}

fn thread_entry(
    mount_id: &MountId,
    parent_path: &Path,
    mailbox: &str,
    thread: GmailThread,
) -> TreeEntry {
    let title = thread
        .messages
        .first()
        .map(message_subject)
        .unwrap_or_else(|| "(no subject)".to_string());
    let version = thread_remote_version(&thread);
    let path = parent_path
        .join(thread_directory_name(&thread, &title))
        .join("page.md");
    let bundle = GmailThreadNativeBundle {
        mailbox: mailbox.to_string(),
        thread: thread.clone(),
    };
    let stub_frontmatter = render_gmail_thread(&bundle)
        .ok()
        .map(|rendered| rendered.document.frontmatter);

    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: thread_remote_id(mailbox, &thread.id),
        kind: EntityKind::Page,
        title,
        path,
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(version),
        stub_frontmatter,
    }
}

fn message_subject(message: &GmailMessage) -> String {
    message
        .payload
        .as_ref()
        .map(header_map)
        .and_then(|headers| headers.get("subject").cloned())
        .filter(|subject| !subject.trim().is_empty())
        .unwrap_or_else(|| "(no subject)".to_string())
}

fn message_filename(message: &GmailMessage, title: &str) -> String {
    let date = message.internal_date.as_deref().unwrap_or("unknown");
    format!(
        "{}-{}-{}.md",
        safe_slug(date),
        safe_slug(title),
        safe_slug(&message.id)
    )
}

fn thread_directory_name(thread: &GmailThread, title: &str) -> String {
    let date = thread
        .messages
        .iter()
        .filter_map(|message| message.internal_date.as_deref())
        .min()
        .unwrap_or("unknown");
    format!(
        "{}-{}-{}",
        safe_slug(date),
        safe_slug(title),
        safe_slug(&thread.id)
    )
}

fn thread_starts_in_date_window(settings: &GmailMountSettings, thread: &GmailThread) -> bool {
    let Some(window) = settings.gmail.date_window.as_ref() else {
        return true;
    };
    let Some(start_date) = thread_start_utc_date_key(thread) else {
        return true;
    };
    let after = gmail_search_date_key(window.after().as_str());
    let before = gmail_search_date_key(window.before().as_str());
    start_date >= after && start_date < before
}

fn thread_start_utc_date_key(thread: &GmailThread) -> Option<i32> {
    thread
        .messages
        .iter()
        .filter_map(|message| message.internal_date.as_deref())
        .filter_map(gmail_internal_date_utc_key)
        .min()
}

fn gmail_search_date_key(value: &str) -> i32 {
    let year = value[0..4].parse::<i32>().unwrap_or(0);
    let month = value[5..7].parse::<i32>().unwrap_or(0);
    let day = value[8..10].parse::<i32>().unwrap_or(0);
    year * 10_000 + month * 100 + day
}

fn gmail_internal_date_utc_key(value: &str) -> Option<i32> {
    let millis = value.parse::<i64>().ok()?;
    let days = millis.div_euclid(86_400_000);
    let (year, month, day) = civil_date_from_unix_days(days);
    Some(year * 10_000 + month as i32 * 100 + day as i32)
}

fn civil_date_from_unix_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

fn mailbox_from_labels(labels: &[String]) -> &'static str {
    if labels.iter().any(|label| label == "SENT") {
        "sent"
    } else if labels.iter().any(|label| label == "DRAFT") {
        "draft"
    } else {
        "inbox"
    }
}

fn mailbox_folder_id(mailbox: &str) -> &'static str {
    match mailbox {
        "sent" => SENT_FOLDER_ID,
        "draft" => DRAFT_FOLDER_ID,
        _ => INBOX_FOLDER_ID,
    }
}

fn outbound_target_from_create(
    parent_id: &RemoteId,
    parent_kind: &Option<EntityKind>,
    parent_workspace: bool,
    source_path: &Path,
) -> LocalityResult<OutboundTarget> {
    if parent_kind.as_ref() != Some(&EntityKind::Directory) || parent_workspace {
        return Err(LocalityError::Unsupported("gmail create parent"));
    }
    match parent_id.as_str() {
        DRAFT_FOLDER_ID if is_direct_child_of(source_path, "draft") => Ok(OutboundTarget::Draft),
        DRAFT_FOLDER_ID => Err(LocalityError::Unsupported("gmail draft source path")),
        OUTBOX_FOLDER_ID if is_direct_child_of(source_path, "outbox") => Ok(OutboundTarget::Send),
        OUTBOX_FOLDER_ID => Err(LocalityError::Unsupported("gmail outbox source path")),
        _ => Err(LocalityError::Unsupported("gmail create parent")),
    }
}

fn is_direct_child_of(path: &Path, directory: &str) -> bool {
    let mut components = path.components();
    matches!(
        components.next(),
        Some(Component::Normal(component)) if component == OsStr::new(directory)
    ) && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
}

fn safe_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "untitled".to_string()
    } else {
        slug.to_string()
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawDraftFrontmatter {
    title: Option<String>,
    to: Option<RawRecipients>,
    cc: Option<RawRecipients>,
    bcc: Option<RawRecipients>,
    subject: Option<String>,
    attachment: Option<yaml_serde::Value>,
    attachments: Option<yaml_serde::Value>,
    gmail: Option<RawDraftGmailFrontmatter>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDraftGmailFrontmatter {
    attachment: Option<yaml_serde::Value>,
    attachments: Option<yaml_serde::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawRecipients {
    One(String),
    Many(Vec<String>),
}

fn parse_gmail_draft_document(document: &CanonicalDocument) -> LocalityResult<GmailDraftDocument> {
    let frontmatter = if document.frontmatter.trim().is_empty() {
        RawDraftFrontmatter::default()
    } else {
        yaml_serde::from_str::<RawDraftFrontmatter>(&document.frontmatter).map_err(|error| {
            LocalityError::Validation(vec![ValidationIssue::new(
                "gmail_draft_frontmatter_invalid",
                PathBuf::new(),
                Some(1),
                format!("Gmail draft frontmatter is invalid: {error}"),
                Some("fix the YAML frontmatter".to_string()),
            )])
        })?
    };
    if raw_draft_frontmatter_has_attachments(&frontmatter) {
        return Err(LocalityError::Unsupported("gmail attachments"));
    }
    Ok(GmailDraftDocument {
        to: frontmatter.to.map(raw_recipients).unwrap_or_default(),
        cc: frontmatter.cc.map(raw_recipients).unwrap_or_default(),
        bcc: frontmatter.bcc.map(raw_recipients).unwrap_or_default(),
        subject: frontmatter
            .subject
            .or(frontmatter.title)
            .unwrap_or_default(),
        body: document.body.clone(),
    })
}

fn raw_draft_frontmatter_has_attachments(frontmatter: &RawDraftFrontmatter) -> bool {
    raw_attachment_value_has_metadata(frontmatter.attachment.as_ref())
        || raw_attachment_value_has_metadata(frontmatter.attachments.as_ref())
        || frontmatter.gmail.as_ref().is_some_and(|gmail| {
            raw_attachment_value_has_metadata(gmail.attachment.as_ref())
                || raw_attachment_value_has_metadata(gmail.attachments.as_ref())
        })
}

fn raw_attachment_value_has_metadata(value: Option<&yaml_serde::Value>) -> bool {
    match value {
        None | Some(yaml_serde::Value::Null) => false,
        Some(yaml_serde::Value::String(value)) => !value.trim().is_empty(),
        Some(yaml_serde::Value::Sequence(values)) => values
            .iter()
            .any(|value| raw_attachment_value_has_metadata(Some(value))),
        Some(yaml_serde::Value::Mapping(values)) => !values.is_empty(),
        Some(yaml_serde::Value::Tagged(tagged)) => {
            raw_attachment_value_has_metadata(Some(&tagged.value))
        }
        Some(yaml_serde::Value::Bool(_) | yaml_serde::Value::Number(_)) => true,
    }
}

fn raw_recipients(value: RawRecipients) -> Vec<String> {
    match value {
        RawRecipients::One(value) => vec![value],
        RawRecipients::Many(values) => values,
    }
}

fn draft_from_push_create(
    title: &str,
    properties: &BTreeMap<String, PropertyValue>,
    body: &str,
) -> LocalityResult<GmailDraftDocument> {
    if draft_properties_have_attachments(properties) {
        return Err(LocalityError::Unsupported("gmail attachments"));
    }
    Ok(GmailDraftDocument {
        to: recipients_property(properties, "to"),
        cc: recipients_property(properties, "cc"),
        bcc: recipients_property(properties, "bcc"),
        subject: string_property(properties, "subject")
            .filter(|subject| !subject.trim().is_empty())
            .unwrap_or_else(|| title.to_string()),
        body: body.to_string(),
    })
}

fn draft_properties_have_attachments(properties: &BTreeMap<String, PropertyValue>) -> bool {
    property_value_has_attachment_metadata(properties.get("attachments"))
        || property_value_has_attachment_metadata(properties.get("attachment"))
        || matches!(
            properties.get("gmail"),
            Some(PropertyValue::Object(gmail))
                if property_value_has_attachment_metadata(gmail.get("attachments"))
                    || property_value_has_attachment_metadata(gmail.get("attachment"))
        )
}

fn property_value_has_attachment_metadata(value: Option<&PropertyValue>) -> bool {
    match value {
        None | Some(PropertyValue::Null) => false,
        Some(PropertyValue::String(value)) => !value.trim().is_empty(),
        Some(PropertyValue::List(values)) => values.iter().any(|value| !value.trim().is_empty()),
        Some(PropertyValue::Array(values)) => values
            .iter()
            .any(|value| property_value_has_attachment_metadata(Some(value))),
        Some(PropertyValue::Object(values)) => !values.is_empty(),
        Some(PropertyValue::Bool(_) | PropertyValue::Number(_)) => true,
    }
}

fn recipients_property(properties: &BTreeMap<String, PropertyValue>, key: &str) -> Vec<String> {
    match properties.get(key) {
        Some(PropertyValue::List(values)) => values.clone(),
        Some(PropertyValue::String(value)) => vec![value.clone()],
        _ => Vec::new(),
    }
}

fn string_property(properties: &BTreeMap<String, PropertyValue>, key: &str) -> Option<String> {
    match properties.get(key) {
        Some(PropertyValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn non_empty_string_property(
    properties: &BTreeMap<String, PropertyValue>,
    key: &str,
) -> Option<String> {
    string_property(properties, key).filter(|value| !value.trim().is_empty())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DraftNative {
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    body: String,
}

impl From<GmailDraftDocument> for DraftNative {
    fn from(value: GmailDraftDocument) -> Self {
        Self {
            to: value.to,
            cc: value.cc,
            bcc: value.bcc,
            subject: value.subject,
            body: value.body,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use locality_connector::{
        ChildContainer, Connector, EnumerateRequest, FetchRequest, ListChildrenRequest,
        ObserveRequest,
    };
    use locality_core::LocalityError;
    use locality_core::journal::{JournalApplyEffect, PushId, PushOperationId};
    use locality_core::model::{CanonicalDocument, EntityKind, MountId, RemoteId};
    use locality_core::planner::{PropertyValue, PushOperation, PushPlan};
    use locality_core::push::RemotePrecondition;
    use locality_core::search::RAW_SEARCH_METADATA_KEY;

    use super::{GmailConfig, GmailConnector};
    use crate::client::GmailApi;
    use crate::dto::{
        GmailDraft, GmailDraftCreateRequest, GmailDraftList, GmailDraftRef, GmailDraftSendRequest,
        GmailDraftUpdateRequest, GmailMessage, GmailMessageList, GmailMessagePartBody,
        GmailMessageRef, GmailMessageSendRequest, GmailThread, GmailThreadList,
    };
    use crate::settings::GmailMountSettings;

    #[test]
    fn enumerate_projects_four_folders_and_recent_inbox_sent_draft_messages() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());

        let entries = connector
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("gmail-main"),
                cursor: None,
            })
            .expect("enumerate");

        assert!(
            entries
                .iter()
                .any(|entry| entry.path == std::path::PathBuf::from("inbox"))
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == std::path::PathBuf::from("sent"))
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == std::path::PathBuf::from("draft"))
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == std::path::PathBuf::from("outbox"))
        );
        assert!(entries.iter().any(|entry| entry.path.starts_with("inbox/")));
        assert!(entries.iter().any(|entry| entry.path.starts_with("sent/")));
        assert!(entries.iter().any(|entry| entry.path.starts_with("draft/")));
        assert!(entries.iter().any(|entry| entry.remote_id
            == RemoteId::new("gmail-draft:draft-1")
            && entry.path.starts_with("draft/")));
        assert!(
            !entries
                .iter()
                .any(|entry| entry.path != std::path::PathBuf::from("outbox")
                    && entry.path.starts_with("outbox"))
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.list_max_results, vec![100, 100]);
        assert_eq!(calls.draft_list_max_results, vec![100]);
    }

    #[test]
    fn enumerate_with_date_window_pages_all_matching_messages_with_gmail_query() {
        let api = Arc::new(FakeGmailApi::default());
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.paged_message_ids.insert(
                ("INBOX".to_string(), None),
                GmailMessageList {
                    messages: vec![GmailMessageRef {
                        id: "inbox-msg-1".to_string(),
                        thread_id: Some("thread-1".to_string()),
                    }],
                    next_page_token: Some("next-inbox".to_string()),
                    result_size_estimate: Some(2),
                },
            );
            calls.paged_message_ids.insert(
                ("INBOX".to_string(), Some("next-inbox".to_string())),
                GmailMessageList {
                    messages: vec![GmailMessageRef {
                        id: "inbox-msg-2".to_string(),
                        thread_id: Some("thread-2".to_string()),
                    }],
                    next_page_token: None,
                    result_size_estimate: Some(2),
                },
            );
        }
        let settings =
            crate::settings::GmailMountSettings::with_date_window("2026-07-01", "2026-07-15")
                .expect("date window");
        let connector = GmailConnector::with_api(
            GmailConfig::new("token").with_settings(settings),
            api.clone(),
        );

        let entries = connector
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("gmail-main"),
                cursor: None,
            })
            .expect("enumerate");

        assert!(
            entries
                .iter()
                .any(|entry| entry.remote_id == RemoteId::new("inbox-msg-1"))
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.remote_id == RemoteId::new("inbox-msg-2"))
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(
            calls.list_queries,
            vec![
                "after:2026/07/01 before:2026/07/15".to_string(),
                "after:2026/07/01 before:2026/07/15".to_string(),
                "after:2026/07/01 before:2026/07/15".to_string(),
            ]
        );
        assert_eq!(
            calls.draft_list_queries,
            vec!["after:2026/07/01 before:2026/07/15".to_string()]
        );
        assert_eq!(
            calls.list_page_tokens,
            vec![None, Some("next-inbox".to_string()), None]
        );
        assert_eq!(calls.draft_list_page_tokens, vec![None]);
    }

    #[test]
    fn enumerate_without_date_window_keeps_recent_100_single_page_behavior() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());

        connector
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("gmail-main"),
                cursor: None,
            })
            .expect("enumerate");

        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.list_max_results, vec![100, 100]);
        assert_eq!(calls.draft_list_max_results, vec![100]);
        assert_eq!(calls.list_page_tokens, vec![None, None]);
        assert_eq!(calls.draft_list_page_tokens, vec![None]);
        assert!(calls.list_queries.is_empty());
        assert!(calls.draft_list_queries.is_empty());
    }

    #[test]
    fn enumerate_without_date_window_reads_only_first_draft_page() {
        let api = Arc::new(FakeGmailApi::default());
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.paged_drafts.insert(
                None,
                GmailDraftList {
                    drafts: vec![GmailDraftRef {
                        id: "draft-1".to_string(),
                        message: message_fixture("draft-msg-1"),
                    }],
                    next_page_token: Some("next-draft".to_string()),
                    result_size_estimate: Some(2),
                },
            );
            calls.paged_drafts.insert(
                Some("next-draft".to_string()),
                GmailDraftList {
                    drafts: vec![GmailDraftRef {
                        id: "draft-2".to_string(),
                        message: message_fixture("draft-msg-2"),
                    }],
                    next_page_token: None,
                    result_size_estimate: Some(2),
                },
            );
        }
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());

        let entries = connector
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("gmail-main"),
                cursor: None,
            })
            .expect("enumerate");

        assert!(
            entries
                .iter()
                .any(|entry| entry.remote_id == RemoteId::new("gmail-draft:draft-1"))
        );
        assert!(
            !entries
                .iter()
                .any(|entry| entry.remote_id == RemoteId::new("gmail-draft:draft-2"))
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.draft_list_page_tokens, vec![None]);
        assert!(calls.draft_list_queries.is_empty());
    }

    #[test]
    fn enumerate_with_date_window_rejects_repeated_page_token() {
        let api = Arc::new(FakeGmailApi::default());
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.panic_after_list_calls = Some(2);
            calls.paged_message_ids.insert(
                ("INBOX".to_string(), None),
                GmailMessageList {
                    messages: vec![GmailMessageRef {
                        id: "inbox-msg-1".to_string(),
                        thread_id: Some("thread-1".to_string()),
                    }],
                    next_page_token: Some("same-token".to_string()),
                    result_size_estimate: Some(2),
                },
            );
            calls.paged_message_ids.insert(
                ("INBOX".to_string(), Some("same-token".to_string())),
                GmailMessageList {
                    messages: vec![GmailMessageRef {
                        id: "inbox-msg-2".to_string(),
                        thread_id: Some("thread-2".to_string()),
                    }],
                    next_page_token: Some("same-token".to_string()),
                    result_size_estimate: Some(2),
                },
            );
        }
        let settings =
            GmailMountSettings::with_date_window("2026-07-01", "2026-07-15").expect("settings");
        let connector = GmailConnector::with_api(
            GmailConfig::new("token").with_settings(settings),
            api.clone(),
        );

        let error = connector
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("gmail-main"),
                cursor: None,
            })
            .expect_err("repeated page token should fail");

        let message = error.to_string();
        assert!(message.contains("repeated page token"));
        assert!(message.contains("same-token"));
    }

    #[test]
    fn enumerate_with_date_window_rejects_repeated_draft_page_token() {
        let api = Arc::new(FakeGmailApi::default());
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.panic_after_draft_list_calls = Some(2);
            calls.paged_drafts.insert(
                None,
                GmailDraftList {
                    drafts: vec![GmailDraftRef {
                        id: "draft-1".to_string(),
                        message: message_fixture("draft-msg-1"),
                    }],
                    next_page_token: Some("same-draft-token".to_string()),
                    result_size_estimate: Some(2),
                },
            );
            calls.paged_drafts.insert(
                Some("same-draft-token".to_string()),
                GmailDraftList {
                    drafts: vec![GmailDraftRef {
                        id: "draft-2".to_string(),
                        message: message_fixture("draft-msg-2"),
                    }],
                    next_page_token: Some("same-draft-token".to_string()),
                    result_size_estimate: Some(2),
                },
            );
        }
        let settings =
            GmailMountSettings::with_date_window("2026-07-01", "2026-07-15").expect("settings");
        let connector = GmailConnector::with_api(
            GmailConfig::new("token").with_settings(settings),
            api.clone(),
        );

        let error = connector
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("gmail-main"),
                cursor: None,
            })
            .expect_err("repeated draft page token should fail");

        let message = error.to_string();
        assert!(message.contains("repeated page token"));
        assert!(message.contains("same-draft-token"));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(
            calls.draft_list_page_tokens,
            vec![None, Some("same-draft-token".to_string())]
        );
    }

    #[test]
    fn enumerate_projects_threads_when_thread_view_enabled() {
        let api = Arc::new(FakeGmailApi::default());
        let settings = crate::settings::GmailMountSettings::default()
            .with_view(crate::settings::GmailProjectionView::Threads);
        let connector = GmailConnector::with_api(
            GmailConfig::new("token").with_settings(settings),
            api.clone(),
        );

        let entries = connector
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("gmail-main"),
                cursor: None,
            })
            .expect("enumerate");

        assert!(
            entries
                .iter()
                .any(|entry| entry.remote_id == RemoteId::new("gmail-thread:inbox:thread-inbox-1"))
        );
        assert!(entries.iter().any(|entry| entry.path
            == std::path::PathBuf::from("inbox/1720900000000-hello-thread-inbox-1/page.md")));
        assert!(
            entries
                .iter()
                .any(|entry| entry.remote_id == RemoteId::new("gmail-thread:sent:thread-sent-1"))
        );
        assert!(entries.iter().any(|entry| entry.remote_id
            == RemoteId::new("gmail-draft:draft-1")
            && entry.path.starts_with("draft/")));
        let calls = api.calls.lock().expect("calls");
        assert!(!calls.message_list_labels.contains(&"DRAFT".to_string()));
        assert_eq!(calls.draft_list_max_results, vec![100]);
    }

    #[test]
    fn list_children_for_draft_folder_returns_remote_drafts() {
        let api = Arc::new(FakeGmailApi::default());
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.paged_drafts.insert(
                None,
                GmailDraftList {
                    drafts: vec![GmailDraftRef {
                        id: "draft-1".to_string(),
                        message: GmailMessage {
                            id: "draft-msg-1".to_string(),
                            thread_id: Some("draft-msg-1-thread".to_string()),
                            label_ids: vec!["DRAFT".to_string()],
                            snippet: None,
                            internal_date: None,
                            payload: None,
                            raw: None,
                        },
                    }],
                    next_page_token: None,
                    result_size_estimate: Some(1),
                },
            );
            calls.draft_full.insert(
                "draft-1".to_string(),
                GmailDraft {
                    id: "draft-1".to_string(),
                    message: message_fixture("draft-msg-1"),
                },
            );
        }
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("gmail-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new("gmail-folder:draft")),
                parent_path: "draft".into(),
            })
            .expect("list draft");

        assert_eq!(result.entries.len(), 1);
        assert_eq!(
            result.entries[0].remote_id,
            RemoteId::new("gmail-draft:draft-1")
        );
        assert_eq!(result.entries[0].title, "Hello");
        assert_eq!(
            result.entries[0].path,
            std::path::PathBuf::from("draft/1720900000000-hello-draft-msg-1.md")
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.draft_list_max_results, vec![100]);
        assert_eq!(calls.draft_full_ids, vec!["draft-1".to_string()]);
        assert!(!calls.message_list_labels.contains(&"DRAFT".to_string()));
    }

    #[test]
    fn list_children_for_outbox_folder_is_empty_staging_surface() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("gmail-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new("gmail-folder:outbox")),
                parent_path: "outbox".into(),
            })
            .expect("list outbox");

        assert!(result.entries.is_empty());
    }

    #[test]
    fn list_children_for_thread_page_returns_message_files() {
        let api = Arc::new(FakeGmailApi::default());
        let settings = crate::settings::GmailMountSettings::default()
            .with_view(crate::settings::GmailProjectionView::Threads);
        let connector =
            GmailConnector::with_api(GmailConfig::new("token").with_settings(settings), api);

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("gmail-main"),
                container: ChildContainer::PageChildren(RemoteId::new(
                    "gmail-thread:inbox:thread-inbox-1",
                )),
                parent_path: "inbox/1720900000000-hello-thread-inbox-1".into(),
            })
            .expect("children");

        assert_eq!(result.entries.len(), 1);
        assert_eq!(
            result.entries[0].remote_id,
            crate::render::thread_message_remote_id("inbox", "thread-inbox-1", "inbox-msg-1")
        );
        assert_eq!(
            result.entries[0].path,
            std::path::PathBuf::from(
                "inbox/1720900000000-hello-thread-inbox-1/1720900000000-hello-inbox-msg-1.md"
            )
        );
    }

    #[test]
    fn thread_child_message_remote_ids_are_namespaced_by_mailbox_and_thread() {
        let api = Arc::new(FakeGmailApi::default());
        let settings = crate::settings::GmailMountSettings::default()
            .with_view(crate::settings::GmailProjectionView::Threads);
        let connector =
            GmailConnector::with_api(GmailConfig::new("token").with_settings(settings), api);

        let inbox_children = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("gmail-main"),
                container: ChildContainer::PageChildren(RemoteId::new(
                    "gmail-thread:inbox:thread-shared",
                )),
                parent_path: "inbox/1720900000000-hello-thread-shared".into(),
            })
            .expect("inbox children");
        let sent_children = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("gmail-main"),
                container: ChildContainer::PageChildren(RemoteId::new(
                    "gmail-thread:sent:thread-shared",
                )),
                parent_path: "sent/1720900000000-hello-thread-shared".into(),
            })
            .expect("sent children");

        assert_eq!(
            inbox_children.entries[0].remote_id,
            crate::render::thread_message_remote_id("inbox", "thread-shared", "inbox-msg-1")
        );
        assert_eq!(
            sent_children.entries[0].remote_id,
            crate::render::thread_message_remote_id("sent", "thread-shared", "inbox-msg-1")
        );
        assert_ne!(
            inbox_children.entries[0].remote_id,
            sent_children.entries[0].remote_id
        );
    }

    #[test]
    fn fetch_returns_thread_native_entity_for_thread_remote_id() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);
        let remote_id = RemoteId::new("gmail-thread:inbox:thread-inbox-1");

        let native = connector
            .fetch(FetchRequest {
                remote_id: remote_id.clone(),
            })
            .expect("fetch thread");

        assert_eq!(native.remote_id, remote_id);
        assert_eq!(native.kind, "gmail_thread");
        let bundle: crate::render::GmailThreadNativeBundle =
            serde_json::from_slice(&native.raw).expect("thread bundle");
        assert_eq!(bundle.mailbox, "inbox");
        assert_eq!(bundle.thread.id, "thread-inbox-1");
        assert_eq!(bundle.thread.messages[0].id, "inbox-msg-1");
    }

    #[test]
    fn fetch_and_render_thread_child_message_preserves_namespaced_remote_id() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);
        let remote_id =
            crate::render::thread_message_remote_id("inbox", "thread-inbox-1", "inbox-msg-1");

        let native = connector
            .fetch(FetchRequest {
                remote_id: remote_id.clone(),
            })
            .expect("fetch thread child message");
        assert_eq!(native.remote_id, remote_id);
        assert_eq!(native.kind, "gmail_thread_message");

        let rendered = connector
            .render(&native)
            .expect("render thread child message");
        assert!(
            rendered
                .frontmatter
                .contains(&format!("id: \"{}\"", remote_id.as_str()))
        );
        assert!(rendered.frontmatter.contains("message_id: \"inbox-msg-1\""));
    }

    #[test]
    fn fetch_remote_draft_uses_draft_resource_and_renders_draft_id() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let remote_id = RemoteId::new("gmail-draft:draft-1");

        let native = connector
            .fetch(FetchRequest {
                remote_id: remote_id.clone(),
            })
            .expect("fetch draft");

        assert_eq!(native.remote_id, remote_id);
        assert_eq!(native.kind, "gmail_message");
        let rendered = connector.render(&native).expect("render draft");
        assert!(rendered.frontmatter.contains("id: \"gmail-draft:draft-1\""));
        assert!(rendered.frontmatter.contains("mailbox: \"draft\""));
        assert!(rendered.frontmatter.contains("draft_id: \"draft-1\""));
        assert!(rendered.frontmatter.contains("message_id: \"draft-msg-1\""));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.draft_full_ids, vec!["draft-1".to_string()]);
        assert!(calls.message_full_ids.is_empty());
    }

    #[test]
    fn fetch_legacy_draft_message_remote_id_still_works() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());

        let native = connector
            .fetch(FetchRequest {
                remote_id: RemoteId::new("draft-msg-1"),
            })
            .expect("fetch legacy draft message");

        let rendered = connector.render(&native).expect("render legacy draft");
        assert!(rendered.frontmatter.contains("id: \"draft-msg-1\""));
        assert!(rendered.frontmatter.contains("mailbox: \"draft\""));
        assert!(!rendered.frontmatter.contains("draft_id:"));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.message_full_ids, vec!["draft-msg-1".to_string()]);
        assert!(calls.draft_full_ids.is_empty());
    }

    #[test]
    fn observe_thread_remote_id_returns_thread_page_metadata() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);
        let remote_id = RemoteId::new("gmail-thread:inbox:thread-inbox-1");

        let observation = connector
            .observe(ObserveRequest {
                mount_id: MountId::new("gmail-main"),
                remote_id: remote_id.clone(),
            })
            .expect("observe thread");

        assert_eq!(observation.remote_id, remote_id);
        assert_eq!(
            observation.parent_remote_id,
            Some(RemoteId::new("gmail-folder:inbox"))
        );
        assert_eq!(observation.title, "Hello");
        assert_eq!(
            observation.projected_path,
            std::path::PathBuf::from("inbox/1720900000000-hello-thread-inbox-1/page.md")
        );
        assert!(observation.raw_metadata_json.contains("thread-inbox-1"));
        let raw_metadata: serde_json::Value =
            serde_json::from_str(&observation.raw_metadata_json).expect("raw metadata json");
        assert_eq!(
            raw_metadata[RAW_SEARCH_METADATA_KEY]["source_url"],
            serde_json::json!("https://mail.google.com/mail/u/0/#all/thread-inbox-1")
        );
        assert_eq!(
            raw_metadata[RAW_SEARCH_METADATA_KEY]["aliases"],
            serde_json::json!(["thread-inbox-1", "inbox-msg-1"])
        );
        let search_terms = raw_metadata[RAW_SEARCH_METADATA_KEY]["metadata_text"]
            .as_array()
            .expect("metadata_text");
        assert!(search_terms.contains(&serde_json::json!("Ann <ann@example.com>")));
        assert!(search_terms.contains(&serde_json::json!("Hello")));
    }

    #[test]
    fn observe_remote_draft_uses_draft_resource() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let remote_id = RemoteId::new("gmail-draft:draft-1");

        let observation = connector
            .observe(ObserveRequest {
                mount_id: MountId::new("gmail-main"),
                remote_id: remote_id.clone(),
            })
            .expect("observe draft");

        assert_eq!(observation.remote_id, remote_id);
        assert_eq!(
            observation.parent_remote_id,
            Some(RemoteId::new("gmail-folder:draft"))
        );
        assert_eq!(
            observation.projected_path,
            std::path::PathBuf::from("draft/1720900000000-hello-draft-msg-1.md")
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.draft_full_ids, vec!["draft-1".to_string()]);
        assert!(calls.message_metadata_ids.is_empty());
    }

    #[test]
    fn observe_legacy_draft_message_remote_id_still_works() {
        let api = Arc::new(FakeGmailApi::default());
        api.calls
            .lock()
            .expect("calls")
            .message_labels
            .insert("draft-msg-1".to_string(), vec!["DRAFT".to_string()]);
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());

        let observation = connector
            .observe(ObserveRequest {
                mount_id: MountId::new("gmail-main"),
                remote_id: RemoteId::new("draft-msg-1"),
            })
            .expect("observe legacy draft");

        assert_eq!(
            observation.parent_remote_id,
            Some(RemoteId::new("gmail-folder:draft"))
        );
        assert_eq!(observation.remote_id, RemoteId::new("draft-msg-1"));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.message_metadata_ids, vec!["draft-msg-1".to_string()]);
        assert!(calls.draft_full_ids.is_empty());
    }

    #[test]
    fn list_children_for_root_uses_receiving_parent_path() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("gmail-main"),
                container: ChildContainer::Root,
                parent_path: "mail".into(),
            })
            .expect("list root");

        let paths = result
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                std::path::PathBuf::from("mail/inbox"),
                std::path::PathBuf::from("mail/sent"),
                std::path::PathBuf::from("mail/draft"),
                std::path::PathBuf::from("mail/outbox"),
            ]
        );
    }

    #[test]
    fn list_children_for_inbox_uses_requested_mailbox_in_stub_frontmatter() {
        let api = Arc::new(FakeGmailApi::default());
        api.calls.lock().expect("calls").message_labels.insert(
            "inbox-msg-1".to_string(),
            vec!["INBOX".to_string(), "SENT".to_string()],
        );
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("gmail-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new("gmail-folder:inbox")),
                parent_path: "inbox".into(),
            })
            .expect("list inbox");

        let entry = result
            .entries
            .iter()
            .find(|entry| entry.path.starts_with("inbox/"))
            .expect("inbox entry");
        let frontmatter = entry.stub_frontmatter.as_ref().expect("frontmatter");
        assert!(frontmatter.contains("mailbox: \"inbox\""));
        assert!(!frontmatter.contains("mailbox: \"sent\""));
    }

    #[test]
    fn list_children_for_inbox_with_date_window_pages_messages_with_gmail_query() {
        let api = Arc::new(FakeGmailApi::default());
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.paged_message_ids.insert(
                ("INBOX".to_string(), None),
                GmailMessageList {
                    messages: vec![GmailMessageRef {
                        id: "inbox-msg-1".to_string(),
                        thread_id: Some("thread-1".to_string()),
                    }],
                    next_page_token: Some("inbox-page-2".to_string()),
                    result_size_estimate: Some(2),
                },
            );
            calls.paged_message_ids.insert(
                ("INBOX".to_string(), Some("inbox-page-2".to_string())),
                GmailMessageList {
                    messages: vec![GmailMessageRef {
                        id: "inbox-msg-2".to_string(),
                        thread_id: Some("thread-2".to_string()),
                    }],
                    next_page_token: None,
                    result_size_estimate: Some(2),
                },
            );
        }
        let settings =
            GmailMountSettings::with_date_window("2026-07-01", "2026-07-15").expect("settings");
        let connector = GmailConnector::with_api(
            GmailConfig::new("token").with_settings(settings),
            api.clone(),
        );

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("gmail-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new("gmail-folder:inbox")),
                parent_path: "inbox".into(),
            })
            .expect("list inbox");

        assert!(result.entries.iter().any(|entry| {
            entry.remote_id == RemoteId::new("inbox-msg-1") && entry.path.starts_with("inbox/")
        }));
        assert!(result.entries.iter().any(|entry| {
            entry.remote_id == RemoteId::new("inbox-msg-2") && entry.path.starts_with("inbox/")
        }));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(
            calls.list_queries,
            vec![
                "after:2026/07/01 before:2026/07/15".to_string(),
                "after:2026/07/01 before:2026/07/15".to_string(),
            ]
        );
        assert_eq!(
            calls.list_page_tokens,
            vec![None, Some("inbox-page-2".to_string())]
        );
    }

    #[test]
    fn list_children_for_thread_view_date_window_filters_by_thread_start_date() {
        let api = Arc::new(FakeGmailApi::default());
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.paged_thread_ids.insert(
                ("INBOX".to_string(), None),
                GmailThreadList {
                    threads: vec![
                        crate::dto::GmailThreadRef {
                            id: "thread-start-before-window".to_string(),
                            snippet: Some("older start".to_string()),
                            history_id: Some("h-before".to_string()),
                        },
                        crate::dto::GmailThreadRef {
                            id: "thread-start-in-window".to_string(),
                            snippet: Some("inside start".to_string()),
                            history_id: Some("h-inside".to_string()),
                        },
                    ],
                    next_page_token: None,
                    result_size_estimate: Some(2),
                },
            );
            calls.thread_metadata.insert(
                "thread-start-before-window".to_string(),
                thread_fixture_with_messages(
                    "thread-start-before-window",
                    [
                        ("old-start-msg", "1782820800000"),
                        ("matching-later-msg", "1782993600000"),
                    ],
                ),
            );
            calls.thread_metadata.insert(
                "thread-start-in-window".to_string(),
                thread_fixture_with_messages(
                    "thread-start-in-window",
                    [
                        ("window-start-msg", "1782993600000"),
                        ("newer-after-window-msg", "1784548800000"),
                    ],
                ),
            );
        }
        let settings = GmailMountSettings::with_date_window("2026-07-01", "2026-07-15")
            .expect("settings")
            .with_view(crate::settings::GmailProjectionView::Threads);
        let connector = GmailConnector::with_api(
            GmailConfig::new("token").with_settings(settings),
            api.clone(),
        );

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("gmail-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new("gmail-folder:inbox")),
                parent_path: "inbox".into(),
            })
            .expect("list inbox threads");

        assert!(!result.entries.iter().any(|entry| {
            entry.remote_id == RemoteId::new("gmail-thread:inbox:thread-start-before-window")
        }));
        let included = result
            .entries
            .iter()
            .find(|entry| {
                entry.remote_id == RemoteId::new("gmail-thread:inbox:thread-start-in-window")
            })
            .expect("thread whose start is in range");
        assert_eq!(
            included.path,
            std::path::PathBuf::from("inbox/1782993600000-hello-thread-start-in-window/page.md")
        );
        assert!(
            included
                .stub_frontmatter
                .as_ref()
                .expect("thread frontmatter")
                .contains("message_count: 2")
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(
            calls.thread_list_queries,
            vec!["after:2026/07/01 before:2026/07/15".to_string()]
        );
    }

    #[test]
    fn apply_create_entity_creates_unsent_gmail_draft() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-folder:draft")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("gmail-folder:draft"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Hello".to_string(),
                properties: std::collections::BTreeMap::from([
                    (
                        "to".to_string(),
                        PropertyValue::List(vec!["ann@example.com".to_string()]),
                    ),
                    (
                        "subject".to_string(),
                        PropertyValue::String("Explicit subject".to_string()),
                    ),
                ]),
                body: "Body\nSecond line\n".to_string(),
                source_path: "draft/hello.md".into(),
            }],
        );

        let result = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-1".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        assert_eq!(
            result.changed_remote_ids,
            vec![RemoteId::new("gmail-draft:draft-1")]
        );
        assert_eq!(
            result.effects,
            vec![JournalApplyEffect::CreatedEntity {
                operation_id: PushOperationId("op-1".to_string()),
                operation_index: 0,
                parent_id: RemoteId::new("gmail-folder:draft"),
                entity_id: RemoteId::new("gmail-draft:draft-1"),
            }]
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 1);
        assert!(calls.sent_drafts.is_empty());
        assert_eq!(calls.sent_messages, 0);
        let raw = calls.created_draft_raw.last().expect("created draft raw");
        let mime = String::from_utf8(
            URL_SAFE_NO_PAD
                .decode(raw.as_bytes())
                .expect("decode raw mime"),
        )
        .expect("utf8 mime");
        assert!(mime.contains("To: ann@example.com\r\n"));
        assert!(mime.contains("Subject: Explicit subject\r\n"));
        assert!(mime.contains("Message-ID: <"));
        assert!(mime.contains("@locality.local>\r\n"));
        assert!(mime.contains("\r\n\r\nBody\r\nSecond line\r\n"));
    }

    #[test]
    fn apply_create_entity_sends_message_from_outbox_folder() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-folder:outbox")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("gmail-folder:outbox"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Hello".to_string(),
                properties: std::collections::BTreeMap::from([
                    (
                        "to".to_string(),
                        PropertyValue::List(vec!["ann@example.com".to_string()]),
                    ),
                    (
                        "subject".to_string(),
                        PropertyValue::String("Explicit subject".to_string()),
                    ),
                ]),
                body: "Body\nSecond line\n".to_string(),
                source_path: "outbox/hello.md".into(),
            }],
        );

        let result = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-1".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        assert_eq!(result.changed_remote_ids, vec![RemoteId::new("sent-msg-1")]);
        assert!(matches!(
            result.effects.as_slice(),
            [locality_core::journal::JournalApplyEffect::CreatedEntity { parent_id, entity_id, .. }]
                if parent_id == &RemoteId::new("gmail-folder:sent")
                    && entity_id == &RemoteId::new("sent-msg-1")
        ));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 0);
        assert!(calls.sent_drafts.is_empty());
        assert_eq!(calls.sent_messages, 1);
        let raw = calls.sent_message_raw.last().expect("sent message raw");
        let mime = String::from_utf8(
            URL_SAFE_NO_PAD
                .decode(raw.as_bytes())
                .expect("decode raw mime"),
        )
        .expect("utf8 mime");
        assert!(mime.contains("To: ann@example.com\r\n"));
        assert!(mime.contains("Subject: Explicit subject\r\n"));
        assert!(mime.contains("Message-ID: <"));
        assert!(mime.contains("@locality.local>\r\n"));
        assert!(mime.contains("\r\n\r\nBody\r\nSecond line\r\n"));
    }

    #[test]
    fn apply_create_entity_recovers_existing_sent_message_for_outbox_folder_without_duplicate() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let push_id = PushId("push-1".to_string());
        let operation_id = PushOperationId("op-1".to_string());
        let message_id = super::locality_message_id(&push_id, &operation_id);
        api.calls.lock().expect("calls").sent_search_results.insert(
            format!("rfc822msgid:<{message_id}>"),
            "sent-msg-previous".to_string(),
        );
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-folder:outbox")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("gmail-folder:outbox"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Hello".to_string(),
                properties: std::collections::BTreeMap::from([
                    (
                        "to".to_string(),
                        PropertyValue::List(vec!["ann@example.com".to_string()]),
                    ),
                    (
                        "subject".to_string(),
                        PropertyValue::String("Explicit subject".to_string()),
                    ),
                ]),
                body: "Body\n".to_string(),
                source_path: "outbox/hello.md".into(),
            }],
        );

        let result = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &push_id,
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: std::slice::from_ref(&operation_id),
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        assert_eq!(
            result.changed_remote_ids,
            vec![RemoteId::new("sent-msg-previous")]
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 0);
        assert_eq!(calls.sent_messages, 0);
        assert_eq!(
            calls.list_queries,
            vec![format!("rfc822msgid:<{message_id}>")]
        );
    }

    #[test]
    fn apply_create_entity_recovers_sent_message_after_send_error() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let push_id = PushId("push-1".to_string());
        let operation_id = PushOperationId("op-1".to_string());
        let message_id = super::locality_message_id(&push_id, &operation_id);
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.send_message_error = Some(LocalityError::Io(
                "gmail message send timed out".to_string(),
            ));
            calls.sent_search_results_after_send.insert(
                format!("rfc822msgid:<{message_id}>"),
                "sent-msg-recovered".to_string(),
            );
        }
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-folder:outbox")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("gmail-folder:outbox"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Hello".to_string(),
                properties: std::collections::BTreeMap::from([
                    (
                        "to".to_string(),
                        PropertyValue::List(vec!["ann@example.com".to_string()]),
                    ),
                    (
                        "subject".to_string(),
                        PropertyValue::String("Explicit subject".to_string()),
                    ),
                ]),
                body: "Body\n".to_string(),
                source_path: "outbox/hello.md".into(),
            }],
        );

        let result = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &push_id,
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: std::slice::from_ref(&operation_id),
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        assert_eq!(
            result.changed_remote_ids,
            vec![RemoteId::new("sent-msg-recovered")]
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 0);
        assert_eq!(calls.sent_messages, 1);
    }

    #[test]
    fn apply_create_entity_rejects_nested_send_source_path() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-folder:outbox")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("gmail-folder:outbox"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Hello".to_string(),
                properties: std::collections::BTreeMap::from([
                    (
                        "to".to_string(),
                        PropertyValue::List(vec!["ann@example.com".to_string()]),
                    ),
                    (
                        "subject".to_string(),
                        PropertyValue::String("Nested source".to_string()),
                    ),
                ]),
                body: "Body\n".to_string(),
                source_path: "outbox/nested/hello.md".into(),
            }],
        );

        let error = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-1".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect_err("nested outbox source should be unsupported");

        assert!(matches!(error, LocalityError::Unsupported(_)));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 0);
        assert_eq!(calls.sent_messages, 0);
    }

    #[test]
    fn apply_create_entity_recovers_existing_sent_message_by_message_id_without_duplicate() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let push_id = PushId("push-1".to_string());
        let operation_id = PushOperationId("op-1".to_string());
        let message_id = super::locality_message_id(&push_id, &operation_id);
        api.calls.lock().expect("calls").sent_search_results.insert(
            format!("rfc822msgid:<{message_id}>"),
            "sent-msg-previous".to_string(),
        );
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-folder:draft")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("gmail-folder:draft"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Hello".to_string(),
                properties: std::collections::BTreeMap::from([
                    (
                        "to".to_string(),
                        PropertyValue::List(vec!["ann@example.com".to_string()]),
                    ),
                    (
                        "subject".to_string(),
                        PropertyValue::String("Explicit subject".to_string()),
                    ),
                ]),
                body: "Body\n".to_string(),
                source_path: "draft/hello.md".into(),
            }],
        );

        let result = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &push_id,
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: std::slice::from_ref(&operation_id),
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        assert_eq!(
            result.changed_remote_ids,
            vec![RemoteId::new("sent-msg-previous")]
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 0);
        assert!(calls.sent_drafts.is_empty());
        assert_eq!(calls.sent_messages, 0);
        assert_eq!(
            calls.list_queries,
            vec![format!("rfc822msgid:<{message_id}>")]
        );
    }

    #[test]
    fn apply_create_entity_does_not_send_when_send_endpoint_would_fail() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let push_id = PushId("push-1".to_string());
        let operation_id = PushOperationId("op-1".to_string());
        let message_id = super::locality_message_id(&push_id, &operation_id);
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.send_error = Some(LocalityError::Io(
                "gmail draft send response decode failed".to_string(),
            ));
            calls.sent_search_results_after_send.insert(
                format!("rfc822msgid:<{message_id}>"),
                "sent-msg-recovered".to_string(),
            );
        }
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-folder:draft")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("gmail-folder:draft"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Hello".to_string(),
                properties: std::collections::BTreeMap::from([
                    (
                        "to".to_string(),
                        PropertyValue::List(vec!["ann@example.com".to_string()]),
                    ),
                    (
                        "subject".to_string(),
                        PropertyValue::String("Explicit subject".to_string()),
                    ),
                ]),
                body: "Body\n".to_string(),
                source_path: "draft/hello.md".into(),
            }],
        );

        let result = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &push_id,
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: std::slice::from_ref(&operation_id),
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        assert_eq!(
            result.changed_remote_ids,
            vec![RemoteId::new("gmail-draft:draft-1")]
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 1);
        assert!(calls.sent_drafts.is_empty());
        assert_eq!(calls.sent_messages, 0);
        assert_eq!(
            calls.list_queries,
            vec![format!("rfc822msgid:<{message_id}>")]
        );
    }

    #[test]
    fn apply_create_entity_does_not_depend_on_sent_lookup() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.send_error = Some(LocalityError::Io(
                "gmail draft send response decode failed".to_string(),
            ));
            calls.sent_search_error_after_send =
                Some(LocalityError::Io("sent search timed out".to_string()));
        }
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-folder:draft")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("gmail-folder:draft"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Hello".to_string(),
                properties: std::collections::BTreeMap::from([
                    (
                        "to".to_string(),
                        PropertyValue::List(vec!["ann@example.com".to_string()]),
                    ),
                    (
                        "subject".to_string(),
                        PropertyValue::String("Explicit subject".to_string()),
                    ),
                ]),
                body: "Body\n".to_string(),
                source_path: "draft/hello.md".into(),
            }],
        );

        let result = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-1".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("draft creation should not send or query sent mail");

        assert_eq!(
            result.changed_remote_ids,
            vec![RemoteId::new("gmail-draft:draft-1")]
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 1);
        assert!(calls.sent_drafts.is_empty());
        assert_eq!(calls.sent_messages, 0);
    }

    #[test]
    fn apply_create_entity_rejects_nested_draft_source_path() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-folder:draft")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("gmail-folder:draft"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Hello".to_string(),
                properties: std::collections::BTreeMap::from([
                    (
                        "to".to_string(),
                        PropertyValue::List(vec!["ann@example.com".to_string()]),
                    ),
                    (
                        "subject".to_string(),
                        PropertyValue::String("Nested source".to_string()),
                    ),
                ]),
                body: "Body\n".to_string(),
                source_path: "draft/nested/hello.md".into(),
            }],
        );

        let error = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-1".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect_err("nested draft source should be unsupported");

        assert!(matches!(error, LocalityError::Unsupported(_)));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 0);
        assert!(calls.sent_drafts.is_empty());
        assert_eq!(calls.sent_messages, 0);
    }

    #[test]
    fn apply_create_entity_rejects_nested_gmail_attachment_metadata() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-folder:draft")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("gmail-folder:draft"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Hello".to_string(),
                properties: std::collections::BTreeMap::from([
                    (
                        "to".to_string(),
                        PropertyValue::List(vec!["ann@example.com".to_string()]),
                    ),
                    (
                        "subject".to_string(),
                        PropertyValue::String("Explicit subject".to_string()),
                    ),
                    (
                        "gmail".to_string(),
                        PropertyValue::Object(std::collections::BTreeMap::from([(
                            "attachments".to_string(),
                            PropertyValue::List(vec!["invoice.pdf".to_string()]),
                        )])),
                    ),
                ]),
                body: "Body\n".to_string(),
                source_path: "draft/hello.md".into(),
            }],
        );

        let error = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-1".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect_err("nested gmail attachments should be unsupported");

        assert!(matches!(error, LocalityError::Unsupported(_)));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 0);
        assert!(calls.sent_drafts.is_empty());
        assert_eq!(calls.sent_messages, 0);
    }

    #[test]
    fn apply_updates_remote_gmail_draft() {
        let api = Arc::new(FakeGmailApi::default());
        api.calls.lock().expect("calls").draft_full.insert(
            "draft-123".to_string(),
            GmailDraft {
                id: "draft-123".to_string(),
                message: message_fixture("draft-original-msg"),
            },
        );
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let draft_remote_id = RemoteId::new("gmail-draft:draft-123");
        let plan = PushPlan::new(
            vec![draft_remote_id.clone()],
            vec![
                PushOperation::UpdateProperties {
                    entity_id: draft_remote_id.clone(),
                    properties: std::collections::BTreeMap::from([
                        (
                            "to".to_string(),
                            PropertyValue::List(vec!["ann@example.com".to_string()]),
                        ),
                        (
                            "subject".to_string(),
                            PropertyValue::String("Updated subject".to_string()),
                        ),
                    ]),
                },
                PushOperation::UpdateEntityBody {
                    entity_id: draft_remote_id.clone(),
                    body: "Updated body\nSecond line\n".to_string(),
                },
            ],
        );

        let result = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[
                    PushOperationId("op-properties".to_string()),
                    PushOperationId("op-body".to_string()),
                ],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        assert_eq!(result.changed_remote_ids, vec![draft_remote_id]);
        assert!(result.effects.is_empty());
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.draft_full_ids, vec!["draft-123"]);
        assert_eq!(calls.updated_drafts.len(), 1);
        assert_eq!(calls.updated_drafts[0].0, "draft-123");
        assert!(calls.sent_drafts.is_empty());
        assert_eq!(calls.sent_messages, 0);
        let mime = decode_raw_mime(&calls.updated_drafts[0].1);
        assert!(mime.contains("To: ann@example.com\r\n"));
        assert!(mime.contains("Subject: Updated subject\r\n"));
        assert!(!mime.contains("Message-ID: <"));
        assert!(mime.contains("\r\n\r\nUpdated body\r\nSecond line\r\n"));
    }

    #[test]
    fn apply_updates_remote_gmail_draft_preserves_bcc_on_body_only_update() {
        let api = Arc::new(FakeGmailApi::default());
        api.calls.lock().expect("calls").draft_full.insert(
            "draft-123".to_string(),
            GmailDraft {
                id: "draft-123".to_string(),
                message: message_fixture_with_header(
                    "draft-original-msg",
                    "Bcc",
                    "hidden@example.com",
                ),
            },
        );
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let draft_remote_id = RemoteId::new("gmail-draft:draft-123");
        let plan = PushPlan::new(
            vec![draft_remote_id.clone()],
            vec![PushOperation::UpdateEntityBody {
                entity_id: draft_remote_id,
                body: "Body-only update\n".to_string(),
            }],
        );

        connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-body".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        let calls = api.calls.lock().expect("calls");
        let mime = decode_raw_mime(&calls.updated_drafts[0].1);
        assert!(mime.contains("Bcc: hidden@example.com\r\n"));
        assert!(mime.contains("\r\n\r\nBody-only update\r\n"));
    }

    #[test]
    fn apply_updates_remote_gmail_draft_uses_title_fallback_when_subject_null() {
        let api = Arc::new(FakeGmailApi::default());
        api.calls.lock().expect("calls").draft_full.insert(
            "draft-123".to_string(),
            GmailDraft {
                id: "draft-123".to_string(),
                message: message_fixture("draft-original-msg"),
            },
        );
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let draft_remote_id = RemoteId::new("gmail-draft:draft-123");
        let plan = PushPlan::new(
            vec![draft_remote_id.clone()],
            vec![PushOperation::UpdateProperties {
                entity_id: draft_remote_id,
                properties: std::collections::BTreeMap::from([
                    ("subject".to_string(), PropertyValue::Null),
                    (
                        "title".to_string(),
                        PropertyValue::String("Title fallback".to_string()),
                    ),
                ]),
            }],
        );

        connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-properties".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        let calls = api.calls.lock().expect("calls");
        let mime = decode_raw_mime(&calls.updated_drafts[0].1);
        assert!(mime.contains("Subject: Title fallback\r\n"));
    }

    #[test]
    fn apply_updates_remote_gmail_draft_uses_current_title_when_subject_null_without_title_delta() {
        let api = Arc::new(FakeGmailApi::default());
        api.calls.lock().expect("calls").draft_full.insert(
            "draft-123".to_string(),
            GmailDraft {
                id: "draft-123".to_string(),
                message: message_fixture("draft-original-msg"),
            },
        );
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let draft_remote_id = RemoteId::new("gmail-draft:draft-123");
        let plan = PushPlan::new(
            vec![draft_remote_id.clone()],
            vec![PushOperation::UpdateProperties {
                entity_id: draft_remote_id,
                properties: std::collections::BTreeMap::from([(
                    "subject".to_string(),
                    PropertyValue::Null,
                )]),
            }],
        );

        connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-properties".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        let calls = api.calls.lock().expect("calls");
        let mime = decode_raw_mime(&calls.updated_drafts[0].1);
        assert!(mime.contains("Subject: Hello\r\n"));
    }

    #[test]
    fn apply_sends_remote_gmail_draft_moved_to_outbox() {
        let api = Arc::new(FakeGmailApi::default());
        api.calls.lock().expect("calls").draft_full.insert(
            "draft-123".to_string(),
            GmailDraft {
                id: "draft-123".to_string(),
                message: message_fixture("draft-original-msg"),
            },
        );
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let draft_remote_id = RemoteId::new("gmail-draft:draft-123");
        let plan = PushPlan::new(
            vec![draft_remote_id.clone()],
            vec![
                PushOperation::MoveEntity {
                    entity_id: draft_remote_id.clone(),
                    new_parent_id: RemoteId::new("gmail-folder:outbox"),
                    new_parent_kind: EntityKind::Directory,
                    new_title: "Move title subject".to_string(),
                    projected_path: "outbox/move-title-subject.md".into(),
                },
                PushOperation::UpdateProperties {
                    entity_id: draft_remote_id.clone(),
                    properties: std::collections::BTreeMap::from([(
                        "to".to_string(),
                        PropertyValue::List(vec!["ann@example.com".to_string()]),
                    )]),
                },
                PushOperation::UpdateEntityBody {
                    entity_id: draft_remote_id.clone(),
                    body: "Ready to send\n".to_string(),
                },
            ],
        );

        let result = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[
                    PushOperationId("op-move".to_string()),
                    PushOperationId("op-properties".to_string()),
                    PushOperationId("op-body".to_string()),
                ],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        assert_eq!(result.changed_remote_ids, vec![RemoteId::new("sent-msg-1")]);
        assert!(matches!(
            result.effects.as_slice(),
            [
                locality_core::journal::JournalApplyEffect::ArchivedEntity {
                    operation_id: archived_operation_id,
                    operation_index: 0,
                    entity_id: archived_entity_id,
                },
                locality_core::journal::JournalApplyEffect::CreatedEntity {
                    operation_id: created_operation_id,
                    operation_index: 0,
                    parent_id,
                    entity_id: created_entity_id,
                }
            ] if archived_operation_id == &PushOperationId("op-move".to_string())
                && archived_entity_id == &draft_remote_id
                && created_operation_id == &PushOperationId("op-move".to_string())
                && parent_id == &RemoteId::new("gmail-folder:sent")
                && created_entity_id == &RemoteId::new("sent-msg-1")
        ));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.draft_full_ids, vec!["draft-123"]);
        assert_eq!(calls.updated_drafts.len(), 1);
        assert_eq!(calls.updated_drafts[0].0, "draft-123");
        assert_eq!(calls.sent_drafts, vec!["draft-123"]);
        assert_eq!(
            calls.call_log,
            vec![
                "get_draft_full:draft-123".to_string(),
                "update_draft:draft-123".to_string(),
                "send_draft:draft-123".to_string(),
            ]
        );
        let mime = decode_raw_mime(&calls.updated_drafts[0].1);
        assert!(mime.contains("To: ann@example.com\r\n"));
        assert!(mime.contains("Subject: Hello\r\n"));
        assert!(!mime.contains("Subject: Move title subject\r\n"));
        assert!(!mime.contains("Message-ID: <"));
        assert!(mime.contains("\r\n\r\nReady to send\r\n"));
    }

    #[test]
    fn apply_sends_remote_gmail_draft_preserves_current_subject_on_move_only_send() {
        let api = Arc::new(FakeGmailApi::default());
        let mut message = message_fixture("draft-original-msg");
        for header in &mut message.payload.as_mut().expect("payload").headers {
            if header.name.eq_ignore_ascii_case("subject") {
                header.value = "Updated draft subject".to_string();
            }
        }
        api.calls.lock().expect("calls").draft_full.insert(
            "draft-123".to_string(),
            GmailDraft {
                id: "draft-123".to_string(),
                message,
            },
        );
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let draft_remote_id = RemoteId::new("gmail-draft:draft-123");
        let plan = PushPlan::new(
            vec![draft_remote_id.clone()],
            vec![PushOperation::MoveEntity {
                entity_id: draft_remote_id,
                new_parent_id: RemoteId::new("gmail-folder:outbox"),
                new_parent_kind: EntityKind::Directory,
                new_title: "Original projected title".to_string(),
                projected_path: "outbox/original-projected-title.md".into(),
            }],
        );

        connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-move".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        let calls = api.calls.lock().expect("calls");
        let mime = decode_raw_mime(&calls.updated_drafts[0].1);
        assert!(mime.contains("Subject: Updated draft subject\r\n"));
        assert!(!mime.contains("Subject: Original projected title\r\n"));
        assert_eq!(calls.sent_drafts, vec!["draft-123"]);
    }

    #[test]
    fn apply_sends_remote_gmail_draft_uses_move_title_when_subject_blank() {
        let api = Arc::new(FakeGmailApi::default());
        api.calls.lock().expect("calls").draft_full.insert(
            "draft-123".to_string(),
            GmailDraft {
                id: "draft-123".to_string(),
                message: message_fixture("draft-original-msg"),
            },
        );
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let draft_remote_id = RemoteId::new("gmail-draft:draft-123");
        let plan = PushPlan::new(
            vec![draft_remote_id.clone()],
            vec![
                PushOperation::MoveEntity {
                    entity_id: draft_remote_id.clone(),
                    new_parent_id: RemoteId::new("gmail-folder:outbox"),
                    new_parent_kind: EntityKind::Directory,
                    new_title: "Move title fallback".to_string(),
                    projected_path: "outbox/move-title-fallback.md".into(),
                },
                PushOperation::UpdateProperties {
                    entity_id: draft_remote_id,
                    properties: std::collections::BTreeMap::from([(
                        "subject".to_string(),
                        PropertyValue::String(String::new()),
                    )]),
                },
            ],
        );

        connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[
                    PushOperationId("op-move".to_string()),
                    PushOperationId("op-properties".to_string()),
                ],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        let calls = api.calls.lock().expect("calls");
        let mime = decode_raw_mime(&calls.updated_drafts[0].1);
        assert!(mime.contains("Subject: Move title fallback\r\n"));
        assert_eq!(calls.sent_drafts, vec!["draft-123"]);
    }

    #[test]
    fn apply_rejects_move_of_non_draft_gmail_entity() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("sent-msg-1")],
            vec![PushOperation::MoveEntity {
                entity_id: RemoteId::new("sent-msg-1"),
                new_parent_id: RemoteId::new("gmail-folder:outbox"),
                new_parent_kind: EntityKind::Directory,
                new_title: "Send again".to_string(),
                projected_path: "outbox/send-again.md".into(),
            }],
        );

        let error = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-move".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect_err("non-draft Gmail moves should be unsupported");

        assert!(matches!(error, LocalityError::Unsupported(_)));
        let calls = api.calls.lock().expect("calls");
        assert!(calls.updated_drafts.is_empty());
        assert!(calls.sent_drafts.is_empty());
    }

    #[test]
    fn apply_rejects_gmail_draft_move_to_non_outbox_parent() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-draft:draft-123")],
            vec![PushOperation::MoveEntity {
                entity_id: RemoteId::new("gmail-draft:draft-123"),
                new_parent_id: RemoteId::new("gmail-folder:sent"),
                new_parent_kind: EntityKind::Directory,
                new_title: "Wrong parent".to_string(),
                projected_path: "sent/wrong-parent.md".into(),
            }],
        );

        let error = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-move".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect_err("draft move to non-outbox parent should be unsupported");

        assert!(matches!(error, LocalityError::Unsupported(_)));
        let calls = api.calls.lock().expect("calls");
        assert!(calls.updated_drafts.is_empty());
        assert!(calls.sent_drafts.is_empty());
    }

    #[test]
    fn apply_rejects_draft_update_with_attachments() {
        let api = Arc::new(FakeGmailApi::default());
        api.calls.lock().expect("calls").draft_full.insert(
            "draft-123".to_string(),
            GmailDraft {
                id: "draft-123".to_string(),
                message: message_fixture("draft-original-msg"),
            },
        );
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-draft:draft-123")],
            vec![PushOperation::UpdateProperties {
                entity_id: RemoteId::new("gmail-draft:draft-123"),
                properties: std::collections::BTreeMap::from([(
                    "gmail".to_string(),
                    PropertyValue::Object(std::collections::BTreeMap::from([(
                        "attachments".to_string(),
                        PropertyValue::List(vec!["invoice.pdf".to_string()]),
                    )])),
                )]),
            }],
        );

        let error = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-properties".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect_err("draft updates with attachments should be unsupported");

        assert!(matches!(error, LocalityError::Unsupported(_)));
        let calls = api.calls.lock().expect("calls");
        assert!(calls.updated_drafts.is_empty());
        assert!(calls.sent_drafts.is_empty());
    }

    #[test]
    fn apply_allows_empty_draft_attachment_metadata() {
        let api = Arc::new(FakeGmailApi::default());
        api.calls.lock().expect("calls").draft_full.insert(
            "draft-123".to_string(),
            GmailDraft {
                id: "draft-123".to_string(),
                message: message_fixture("draft-original-msg"),
            },
        );
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-draft:draft-123")],
            vec![PushOperation::UpdateProperties {
                entity_id: RemoteId::new("gmail-draft:draft-123"),
                properties: std::collections::BTreeMap::from([
                    (
                        "attachment".to_string(),
                        PropertyValue::String(String::new()),
                    ),
                    ("attachments".to_string(), PropertyValue::List(Vec::new())),
                    (
                        "gmail".to_string(),
                        PropertyValue::Object(std::collections::BTreeMap::from([(
                            "attachments".to_string(),
                            PropertyValue::Array(Vec::new()),
                        )])),
                    ),
                ]),
            }],
        );

        connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-properties".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("empty attachment metadata should be ignored");

        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.draft_full_ids, vec!["draft-123"]);
        assert_eq!(calls.updated_drafts.len(), 1);
        assert!(calls.sent_drafts.is_empty());
    }

    #[test]
    fn apply_rejects_draft_update_when_remote_draft_has_attachments() {
        let api = Arc::new(FakeGmailApi::default());
        api.calls.lock().expect("calls").draft_full.insert(
            "draft-123".to_string(),
            GmailDraft {
                id: "draft-123".to_string(),
                message: message_fixture_with_attachment("draft-original-msg"),
            },
        );
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api.clone());
        let plan = PushPlan::new(
            vec![RemoteId::new("gmail-draft:draft-123")],
            vec![PushOperation::UpdateEntityBody {
                entity_id: RemoteId::new("gmail-draft:draft-123"),
                body: "Edited body\n".to_string(),
            }],
        );

        let error = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-body".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect_err("remote draft attachments should be unsupported");

        assert!(matches!(error, LocalityError::Unsupported(_)));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.draft_full_ids, vec!["draft-123"]);
        assert!(calls.updated_drafts.is_empty());
        assert!(calls.sent_drafts.is_empty());
    }

    #[test]
    fn parse_draft_rejects_nested_gmail_attachment_metadata() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);

        let error = connector
            .parse(&CanonicalDocument::new(
                "to: [\"ann@example.com\"]\nsubject: Hello\ngmail:\n  attachments:\n    - filename: invoice.pdf\n",
                "Body",
            ))
            .expect_err("nested gmail attachments should be unsupported");

        assert!(matches!(error, LocalityError::Unsupported(_)));
    }

    #[test]
    fn parse_draft_allows_empty_attachment_metadata() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);

        connector
            .parse(&CanonicalDocument::new(
                "to: [\"ann@example.com\"]\nsubject: Hello\nattachment: \"\"\nattachments: []\ngmail:\n  attachments: []\n",
                "Body",
            ))
            .expect("empty attachment metadata should be ignored");
    }

    #[test]
    fn parse_invalid_draft_frontmatter_returns_validation_error() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);

        let error = connector
            .parse(&CanonicalDocument::new("subject: [", "Body"))
            .expect_err("invalid frontmatter");

        assert!(matches!(error, LocalityError::Validation(_)));
    }

    #[test]
    fn debug_redacts_connector_access_token() {
        let config = GmailConfig::new("connector-access-token");
        let connector = GmailConnector::new(config.clone());

        let config_debug = format!("{config:?}");
        assert!(!config_debug.contains("connector-access-token"));
        assert!(config_debug.contains("<redacted>"));

        let connector_debug = format!("{connector:?}");
        assert!(!connector_debug.contains("connector-access-token"));
        assert!(connector_debug.contains("<redacted>"));
    }

    #[derive(Default, Debug)]
    struct FakeGmailApi {
        calls: Mutex<FakeCalls>,
    }

    #[derive(Default, Debug)]
    struct FakeCalls {
        message_list_labels: Vec<String>,
        list_max_results: Vec<u32>,
        list_queries: Vec<String>,
        paged_message_ids: std::collections::BTreeMap<(String, Option<String>), GmailMessageList>,
        thread_list_queries: Vec<String>,
        paged_thread_ids: std::collections::BTreeMap<(String, Option<String>), GmailThreadList>,
        thread_metadata: std::collections::BTreeMap<String, GmailThread>,
        list_page_tokens: Vec<Option<String>>,
        draft_list_max_results: Vec<u32>,
        draft_list_page_tokens: Vec<Option<String>>,
        draft_list_queries: Vec<String>,
        paged_drafts: std::collections::BTreeMap<Option<String>, GmailDraftList>,
        draft_full: std::collections::BTreeMap<String, GmailDraft>,
        panic_after_draft_list_calls: Option<usize>,
        panic_after_list_calls: Option<usize>,
        sent_search_results: std::collections::BTreeMap<String, String>,
        sent_search_results_after_send: std::collections::BTreeMap<String, String>,
        send_error: Option<LocalityError>,
        send_message_error: Option<LocalityError>,
        sent_search_error_after_send: Option<LocalityError>,
        message_labels: std::collections::BTreeMap<String, Vec<String>>,
        message_metadata_ids: Vec<String>,
        message_full_ids: Vec<String>,
        draft_full_ids: Vec<String>,
        call_log: Vec<String>,
        created_drafts: usize,
        created_draft_raw: Vec<String>,
        updated_drafts: Vec<(String, String)>,
        sent_drafts: Vec<String>,
        sent_messages: usize,
        sent_message_raw: Vec<String>,
    }

    impl GmailApi for FakeGmailApi {
        fn list_messages(
            &self,
            label_id: &str,
            max_results: u32,
            _page_token: Option<&str>,
            query: Option<&str>,
        ) -> locality_core::LocalityResult<GmailMessageList> {
            let mut calls = self.calls.lock().expect("calls");
            calls.message_list_labels.push(label_id.to_string());
            calls.list_max_results.push(max_results);
            calls.list_page_tokens.push(_page_token.map(str::to_string));
            if let Some(limit) = calls.panic_after_list_calls {
                assert!(
                    calls.list_max_results.len() <= limit,
                    "list_messages exceeded call limit {limit}"
                );
            }
            if let Some(query) = query {
                calls.list_queries.push(query.to_string());
            }
            if let Some(page) = calls
                .paged_message_ids
                .get(&(label_id.to_string(), _page_token.map(str::to_string)))
                .cloned()
            {
                return Ok(page);
            }
            if let Some(sent_message_id) = calls.sent_search_results.get(query.unwrap_or_default())
            {
                return Ok(GmailMessageList {
                    messages: vec![GmailMessageRef {
                        id: sent_message_id.clone(),
                        thread_id: Some(format!("{sent_message_id}-thread")),
                    }],
                    next_page_token: None,
                    result_size_estimate: Some(1),
                });
            }
            let send_attempted = !calls.sent_drafts.is_empty() || calls.sent_messages > 0;
            if send_attempted && let Some(error) = calls.sent_search_error_after_send.clone() {
                return Err(error);
            }
            if send_attempted
                && let Some(sent_message_id) = calls
                    .sent_search_results_after_send
                    .get(query.unwrap_or_default())
            {
                return Ok(GmailMessageList {
                    messages: vec![GmailMessageRef {
                        id: sent_message_id.clone(),
                        thread_id: Some(format!("{sent_message_id}-thread")),
                    }],
                    next_page_token: None,
                    result_size_estimate: Some(1),
                });
            }
            if query.is_some() {
                return Ok(GmailMessageList {
                    messages: Vec::new(),
                    next_page_token: None,
                    result_size_estimate: Some(0),
                });
            }
            let id = match label_id {
                "INBOX" => "inbox-msg-1",
                "SENT" => "sent-msg-1",
                "DRAFT" => "draft-msg-1",
                other => panic!("unexpected label {other}"),
            };
            Ok(GmailMessageList {
                messages: vec![GmailMessageRef {
                    id: id.to_string(),
                    thread_id: Some(format!("{id}-thread")),
                }],
                next_page_token: None,
                result_size_estimate: Some(1),
            })
        }

        fn list_threads(
            &self,
            label_id: &str,
            max_results: u32,
            page_token: Option<&str>,
            query: Option<&str>,
        ) -> locality_core::LocalityResult<GmailThreadList> {
            let _ = max_results;
            let mut calls = self.calls.lock().expect("calls");
            if let Some(query) = query {
                calls.thread_list_queries.push(query.to_string());
            }
            if let Some(page) = calls
                .paged_thread_ids
                .get(&(label_id.to_string(), page_token.map(str::to_string)))
                .cloned()
            {
                return Ok(page);
            }
            if query.is_some() {
                return Ok(GmailThreadList {
                    threads: Vec::new(),
                    next_page_token: None,
                    result_size_estimate: Some(0),
                });
            }
            let id = match label_id {
                "INBOX" => "thread-inbox-1",
                "SENT" => "thread-sent-1",
                other => panic!("unexpected label {other}"),
            };
            Ok(GmailThreadList {
                threads: vec![crate::dto::GmailThreadRef {
                    id: id.to_string(),
                    snippet: Some("hello".to_string()),
                    history_id: Some("h1".to_string()),
                }],
                next_page_token: None,
                result_size_estimate: Some(1),
            })
        }

        fn get_message_metadata(
            &self,
            message_id: &str,
        ) -> locality_core::LocalityResult<GmailMessage> {
            let mut calls = self.calls.lock().expect("calls");
            calls.message_metadata_ids.push(message_id.to_string());
            let labels = calls.message_labels.get(message_id).cloned();
            Ok(message_fixture_with_labels(message_id, labels))
        }

        fn get_message_full(
            &self,
            message_id: &str,
        ) -> locality_core::LocalityResult<GmailMessage> {
            self.calls
                .lock()
                .expect("calls")
                .message_full_ids
                .push(message_id.to_string());
            Ok(message_fixture(message_id))
        }

        fn get_thread_metadata(
            &self,
            thread_id: &str,
        ) -> locality_core::LocalityResult<GmailThread> {
            if let Some(thread) = self
                .calls
                .lock()
                .expect("calls")
                .thread_metadata
                .get(thread_id)
                .cloned()
            {
                return Ok(thread);
            }
            Ok(thread_fixture(thread_id))
        }

        fn get_thread_full(&self, thread_id: &str) -> locality_core::LocalityResult<GmailThread> {
            Ok(thread_fixture(thread_id))
        }

        fn get_attachment(
            &self,
            _message_id: &str,
            _attachment_id: &str,
        ) -> locality_core::LocalityResult<GmailMessagePartBody> {
            Ok(GmailMessagePartBody::default())
        }

        fn list_drafts(
            &self,
            max_results: u32,
            page_token: Option<&str>,
            query: Option<&str>,
        ) -> locality_core::LocalityResult<GmailDraftList> {
            let mut calls = self.calls.lock().expect("calls");
            calls.draft_list_max_results.push(max_results);
            calls
                .draft_list_page_tokens
                .push(page_token.map(str::to_string));
            if let Some(limit) = calls.panic_after_draft_list_calls {
                assert!(
                    calls.draft_list_max_results.len() <= limit,
                    "list_drafts exceeded call limit {limit}"
                );
            }
            if let Some(query) = query {
                calls.draft_list_queries.push(query.to_string());
            }
            if let Some(page) = calls
                .paged_drafts
                .get(&page_token.map(str::to_string))
                .cloned()
            {
                return Ok(page);
            }
            Ok(GmailDraftList {
                drafts: vec![GmailDraftRef {
                    id: "draft-1".to_string(),
                    message: message_fixture("draft-msg-1"),
                }],
                next_page_token: None,
                result_size_estimate: Some(1),
            })
        }

        fn get_draft_full(&self, draft_id: &str) -> locality_core::LocalityResult<GmailDraft> {
            let mut calls = self.calls.lock().expect("calls");
            calls.draft_full_ids.push(draft_id.to_string());
            calls.call_log.push(format!("get_draft_full:{draft_id}"));
            if let Some(draft) = calls.draft_full.get(draft_id).cloned() {
                return Ok(draft);
            }
            Ok(GmailDraft {
                id: draft_id.to_string(),
                message: message_fixture("draft-msg-1"),
            })
        }

        fn create_draft(
            &self,
            request: GmailDraftCreateRequest,
        ) -> locality_core::LocalityResult<GmailDraft> {
            let mut calls = self.calls.lock().expect("calls");
            calls.created_drafts += 1;
            calls.created_draft_raw.push(request.message.raw);
            Ok(GmailDraft {
                id: "draft-1".to_string(),
                message: message_fixture("draft-message-1"),
            })
        }

        fn update_draft(
            &self,
            draft_id: &str,
            request: GmailDraftUpdateRequest,
        ) -> locality_core::LocalityResult<GmailDraft> {
            let mut calls = self.calls.lock().expect("calls");
            calls.call_log.push(format!("update_draft:{draft_id}"));
            calls
                .updated_drafts
                .push((draft_id.to_string(), request.message.raw));
            Ok(GmailDraft {
                id: draft_id.to_string(),
                message: message_fixture(&format!("updated-draft-message-{draft_id}")),
            })
        }

        fn send_message(
            &self,
            request: GmailMessageSendRequest,
        ) -> locality_core::LocalityResult<GmailMessage> {
            let mut calls = self.calls.lock().expect("calls");
            calls.sent_messages += 1;
            calls.sent_message_raw.push(request.raw);
            if let Some(error) = calls.send_message_error.clone() {
                return Err(error);
            }
            Ok(message_fixture("sent-msg-1"))
        }

        fn send_draft(
            &self,
            request: GmailDraftSendRequest,
        ) -> locality_core::LocalityResult<GmailMessage> {
            let mut calls = self.calls.lock().expect("calls");
            calls.call_log.push(format!("send_draft:{}", request.id));
            calls.sent_drafts.push(request.id);
            if let Some(error) = calls.send_error.clone() {
                return Err(error);
            }
            Ok(message_fixture("sent-msg-1"))
        }
    }

    fn message_fixture(id: &str) -> GmailMessage {
        let labels = if id.starts_with("sent") {
            Some(vec!["SENT".to_string()])
        } else if id.starts_with("draft") {
            Some(vec!["DRAFT".to_string()])
        } else {
            Some(vec!["INBOX".to_string()])
        };
        message_fixture_with_labels(id, labels)
    }

    fn decode_raw_mime(raw: &str) -> String {
        String::from_utf8(
            URL_SAFE_NO_PAD
                .decode(raw.as_bytes())
                .expect("decode raw mime"),
        )
        .expect("utf8 mime")
    }

    fn message_fixture_with_attachment(id: &str) -> GmailMessage {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "threadId": format!("{id}-thread"),
            "labelIds": ["DRAFT"],
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "multipart/mixed",
                "headers": [
                    { "name": "From", "value": "Ann <ann@example.com>" },
                    { "name": "To", "value": "me@example.com" },
                    { "name": "Subject", "value": "Hello" },
                    { "name": "Date", "value": "Tue, 14 Jul 2026 09:30:00 +0000" }
                ],
                "parts": [
                    {
                        "mimeType": "text/plain",
                        "body": { "data": "Qm9keQo" }
                    },
                    {
                        "mimeType": "application/pdf",
                        "filename": "invoice.pdf",
                        "body": {
                            "attachmentId": "attachment-1",
                            "size": 12
                        }
                    }
                ]
            }
        }))
        .expect("message with attachment")
    }

    fn thread_fixture(thread_id: &str) -> crate::dto::GmailThread {
        let message_id = if thread_id.contains("sent") {
            "sent-msg-1"
        } else {
            "inbox-msg-1"
        };
        crate::dto::GmailThread {
            id: thread_id.to_string(),
            history_id: Some("h1".to_string()),
            messages: vec![message_fixture(message_id)],
        }
    }

    fn thread_fixture_with_messages<const N: usize>(
        thread_id: &str,
        messages: [(&str, &str); N],
    ) -> crate::dto::GmailThread {
        crate::dto::GmailThread {
            id: thread_id.to_string(),
            history_id: Some("h1".to_string()),
            messages: messages
                .into_iter()
                .map(|(id, internal_date)| {
                    let mut message = message_fixture(id);
                    message.thread_id = Some(thread_id.to_string());
                    message.internal_date = Some(internal_date.to_string());
                    message
                })
                .collect(),
        }
    }

    fn message_fixture_with_labels(id: &str, labels: Option<Vec<String>>) -> GmailMessage {
        let labels = labels.unwrap_or_else(|| {
            if id.starts_with("sent") {
                vec!["SENT".to_string()]
            } else {
                vec!["INBOX".to_string()]
            }
        });
        serde_json::from_value(serde_json::json!({
            "id": id,
            "threadId": format!("{id}-thread"),
            "labelIds": labels,
            "internalDate": "1720900000000",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    { "name": "From", "value": "Ann <ann@example.com>" },
                    { "name": "To", "value": "me@example.com" },
                    { "name": "Subject", "value": "Hello" },
                    { "name": "Date", "value": "Tue, 14 Jul 2026 09:30:00 +0000" }
                ],
                "body": { "data": "Qm9keQo" }
            }
        }))
        .expect("message")
    }

    fn message_fixture_with_header(id: &str, name: &str, value: &str) -> GmailMessage {
        let mut message = message_fixture(id);
        let payload = message.payload.as_mut().expect("payload");
        payload.headers.push(crate::dto::GmailHeader {
            name: name.to_string(),
            value: value.to_string(),
        });
        message
    }
}
