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
    GmailDraftCreateRequest, GmailDraftSendRequest, GmailMessage, GmailRawMessage, GmailThread,
    header_map,
};
use crate::oauth::GMAIL_CONNECTOR_ID;
use crate::render::{
    GmailDraftDocument, GmailNativeBundle, GmailThreadMessageNativeBundle, GmailThreadNativeBundle,
    build_draft_mime_with_message_id, message_frontmatter, parse_thread_message_remote_id,
    parse_thread_remote_id, raw_message_base64url, remote_version, render_gmail_message,
    render_gmail_thread, render_gmail_thread_message, thread_message_remote_id, thread_remote_id,
    thread_remote_version,
};
use crate::settings::{GmailMountSettings, GmailProjectionView};

const GMAIL_PAGE_SIZE: u32 = 100;
const INBOX_FOLDER_ID: &str = "gmail-folder:inbox";
const SENT_FOLDER_ID: &str = "gmail-folder:sent";
const DRAFT_FOLDER_ID: &str = "gmail-folder:draft";

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
        [PushOperationKind::CreateEntity].into_iter().collect()
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

        let message = self.api.get_message_full(request.remote_id.as_str())?;
        let bundle = GmailNativeBundle {
            mailbox: mailbox_from_labels(&message.label_ids).to_string(),
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

        for (index, operation) in request.plan.operations.iter().enumerate() {
            let operation_id =
                request.operation_ids.get(index).cloned().ok_or_else(|| {
                    LocalityError::InvalidState("missing operation id".to_string())
                })?;
            let PushOperation::CreateEntity {
                parent_id,
                parent_kind,
                parent_workspace,
                title,
                properties,
                body,
                source_path,
            } = operation
            else {
                return Err(LocalityError::Unsupported("gmail push operation"));
            };
            if parent_id.as_str() != DRAFT_FOLDER_ID
                || parent_kind.as_ref() != Some(&EntityKind::Directory)
                || *parent_workspace
            {
                return Err(LocalityError::Unsupported("gmail create parent"));
            }
            if !is_direct_draft_child(source_path) {
                return Err(LocalityError::Unsupported("gmail draft source path"));
            }

            let message_id = locality_message_id(request.push_id, &operation_id);
            if let Some(sent) = find_sent_message_by_message_id(self.api.as_ref(), &message_id)? {
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
            let created = self.api.create_draft(GmailDraftCreateRequest {
                message: GmailRawMessage {
                    raw: raw_message_base64url(&mime),
                },
            })?;
            let sent = match self
                .api
                .send_draft(GmailDraftSendRequest { id: created.id })
            {
                Ok(sent) => sent,
                Err(error) => {
                    match find_sent_message_by_message_id(self.api.as_ref(), &message_id) {
                        Ok(Some(sent)) => sent,
                        Ok(None) => return Err(error),
                        Err(lookup_error) => {
                            return Err(LocalityError::Io(format!(
                                "gmail draft send ambiguous after send failure; sent lookup failed: {lookup_error}"
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

fn folder_specs() -> [FolderSpec; 3] {
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

fn is_direct_draft_child(path: &Path) -> bool {
    let mut components = path.components();
    matches!(
        components.next(),
        Some(Component::Normal(component)) if component == OsStr::new("draft")
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
    frontmatter.attachment.is_some()
        || frontmatter.attachments.is_some()
        || frontmatter
            .gmail
            .as_ref()
            .is_some_and(|gmail| gmail.attachment.is_some() || gmail.attachments.is_some())
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
    properties.contains_key("attachments")
        || properties.contains_key("attachment")
        || matches!(
            properties.get("gmail"),
            Some(PropertyValue::Object(gmail))
                if gmail.contains_key("attachments") || gmail.contains_key("attachment")
        )
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
    use locality_core::journal::{PushId, PushOperationId};
    use locality_core::model::{CanonicalDocument, EntityKind, MountId, RemoteId};
    use locality_core::planner::{PropertyValue, PushOperation, PushPlan};
    use locality_core::push::RemotePrecondition;
    use locality_core::search::RAW_SEARCH_METADATA_KEY;

    use super::{GmailConfig, GmailConnector};
    use crate::client::GmailApi;
    use crate::dto::{
        GmailDraft, GmailDraftCreateRequest, GmailDraftSendRequest, GmailMessage, GmailMessageList,
        GmailMessagePartBody, GmailMessageRef, GmailThread, GmailThreadList,
    };
    use crate::settings::GmailMountSettings;

    #[test]
    fn enumerate_projects_three_folders_and_recent_inbox_sent_messages() {
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
        assert!(entries.iter().any(|entry| entry.path.starts_with("inbox/")));
        assert!(entries.iter().any(|entry| entry.path.starts_with("sent/")));
        assert!(!entries
            .iter()
            .any(|entry| entry.path.starts_with("draft") && entry.path.components().count() > 1));
        assert_eq!(
            api.calls.lock().expect("calls").list_max_results,
            vec![100, 100]
        );
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
            calls.list_page_tokens,
            vec![None, Some("next-inbox".to_string()), None]
        );
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
        assert_eq!(calls.list_page_tokens, vec![None, None]);
        assert!(calls.list_queries.is_empty());
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
    }

    #[test]
    fn list_children_for_draft_folder_returns_empty_remote_entries() {
        let api = Arc::new(FakeGmailApi::default());
        let connector = GmailConnector::with_api(GmailConfig::new("token"), api);

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("gmail-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new("gmail-folder:draft")),
                parent_path: "draft".into(),
            })
            .expect("list draft");

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
    fn apply_create_entity_creates_and_sends_gmail_draft() {
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

        assert_eq!(result.changed_remote_ids, vec![RemoteId::new("sent-msg-1")]);
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 1);
        assert_eq!(calls.sent_drafts, vec!["draft-1"]);
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
    fn apply_create_entity_recovers_existing_sent_message_by_message_id_without_resend() {
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
        assert_eq!(
            calls.list_queries,
            vec![format!("rfc822msgid:<{message_id}>")]
        );
    }

    #[test]
    fn apply_create_entity_recovers_sent_message_after_send_response_failure() {
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
            vec![RemoteId::new("sent-msg-recovered")]
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 1);
        assert_eq!(calls.sent_drafts, vec!["draft-1"]);
        assert_eq!(
            calls.list_queries,
            vec![
                format!("rfc822msgid:<{message_id}>"),
                format!("rfc822msgid:<{message_id}>"),
            ]
        );
    }

    #[test]
    fn apply_create_entity_preserves_send_ambiguity_when_recovery_lookup_fails() {
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

        let error = connector
            .apply(locality_connector::ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("gmail-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-1".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect_err("recovery lookup failure should preserve ambiguous send");

        let message = error.to_string();
        assert!(message.contains("gmail draft send"));
        assert!(message.contains("sent search timed out"));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 1);
        assert_eq!(calls.sent_drafts, vec!["draft-1"]);
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
        list_max_results: Vec<u32>,
        list_queries: Vec<String>,
        paged_message_ids: std::collections::BTreeMap<(String, Option<String>), GmailMessageList>,
        thread_list_queries: Vec<String>,
        paged_thread_ids: std::collections::BTreeMap<(String, Option<String>), GmailThreadList>,
        thread_metadata: std::collections::BTreeMap<String, GmailThread>,
        list_page_tokens: Vec<Option<String>>,
        panic_after_list_calls: Option<usize>,
        sent_search_results: std::collections::BTreeMap<String, String>,
        sent_search_results_after_send: std::collections::BTreeMap<String, String>,
        send_error: Option<LocalityError>,
        sent_search_error_after_send: Option<LocalityError>,
        message_labels: std::collections::BTreeMap<String, Vec<String>>,
        created_drafts: usize,
        created_draft_raw: Vec<String>,
        sent_drafts: Vec<String>,
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
            if !calls.sent_drafts.is_empty()
                && let Some(error) = calls.sent_search_error_after_send.clone()
            {
                return Err(error);
            }
            if !calls.sent_drafts.is_empty()
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
            let labels = self
                .calls
                .lock()
                .expect("calls")
                .message_labels
                .get(message_id)
                .cloned();
            Ok(message_fixture_with_labels(message_id, labels))
        }

        fn get_message_full(
            &self,
            message_id: &str,
        ) -> locality_core::LocalityResult<GmailMessage> {
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

        fn send_draft(
            &self,
            request: GmailDraftSendRequest,
        ) -> locality_core::LocalityResult<GmailMessage> {
            let mut calls = self.calls.lock().expect("calls");
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
        } else {
            Some(vec!["INBOX".to_string()])
        };
        message_fixture_with_labels(id, labels)
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
}
