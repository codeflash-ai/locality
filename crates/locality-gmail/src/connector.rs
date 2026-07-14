use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, ChildContainer,
    Connector, ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest,
    ListChildrenRequest, ListChildrenResult, NativeEntity, ObserveRequest, ParsedEntity,
};
use locality_core::freshness::{RemoteObservation, RemoteVersion};
use locality_core::journal::JournalApplyEffect;
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::{PropertyValue, PushOperation, PushOperationKind};
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::client::{GmailApi, HttpGmailApiClient};
use crate::dto::{
    GmailDraftCreateRequest, GmailDraftSendRequest, GmailMessage, GmailRawMessage, header_map,
};
use crate::oauth::GMAIL_CONNECTOR_ID;
use crate::render::{
    GmailDraftDocument, GmailNativeBundle, build_draft_mime, message_frontmatter,
    raw_message_base64url, remote_version, render_gmail_message,
};

const RECENT_LIMIT: u32 = 100;
const INBOX_FOLDER_ID: &str = "gmail-folder:inbox";
const SENT_FOLDER_ID: &str = "gmail-folder:sent";
const DRAFT_FOLDER_ID: &str = "gmail-folder:draft";

#[derive(Clone, PartialEq, Eq)]
pub struct GmailConfig {
    pub access_token: String,
}

impl GmailConfig {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
        }
    }
}

impl fmt::Debug for GmailConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GmailConfig")
            .field("access_token", &"<redacted>")
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
}

impl Connector for GmailConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind(GMAIL_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: false,
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
        let mut entries = gmail_folder_entries(&request.mount_id);
        entries.extend(list_label_entries(
            self.api.as_ref(),
            &request.mount_id,
            "INBOX",
            Path::new("inbox"),
        )?);
        entries.extend(list_label_entries(
            self.api.as_ref(),
            &request.mount_id,
            "SENT",
            Path::new("sent"),
        )?);
        Ok(entries)
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let entries = match request.container {
            ChildContainer::Root => gmail_folder_entries(&request.mount_id),
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == INBOX_FOLDER_ID =>
            {
                list_label_entries(
                    self.api.as_ref(),
                    &request.mount_id,
                    "INBOX",
                    &request.parent_path,
                )?
            }
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == SENT_FOLDER_ID =>
            {
                list_label_entries(
                    self.api.as_ref(),
                    &request.mount_id,
                    "SENT",
                    &request.parent_path,
                )?
            }
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == DRAFT_FOLDER_ID =>
            {
                Vec::new()
            }
            _ => Vec::new(),
        };
        Ok(ListChildrenResult { entries })
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        if let Some(folder) = folder_spec(request.remote_id.as_str()) {
            return Ok(folder_observation(
                request.mount_id,
                &request.remote_id,
                folder,
            ));
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
        .with_raw_metadata_json(
            serde_json::to_string(&message).unwrap_or_else(|_| "{}".to_string()),
        ))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
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
                ..
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

            let draft = draft_from_push_create(title, properties, body)?;
            let mime = build_draft_mime(&draft)?;
            let created = self.api.create_draft(GmailDraftCreateRequest {
                message: GmailRawMessage {
                    raw: raw_message_base64url(&mime),
                },
            })?;
            let sent = self
                .api
                .send_draft(GmailDraftSendRequest { id: created.id })?;
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

fn gmail_folder_entries(mount_id: &MountId) -> Vec<TreeEntry> {
    folder_specs()
        .into_iter()
        .map(|folder| TreeEntry {
            mount_id: mount_id.clone(),
            remote_id: RemoteId::new(folder.id),
            kind: EntityKind::Directory,
            title: folder.title.to_string(),
            path: PathBuf::from(folder.title),
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
    .with_raw_metadata_json(format!(
        r#"{{"kind":"gmail_folder","id":"{}","title":"{}"}}"#,
        folder.id, folder.title
    ))
}

fn list_label_entries(
    api: &dyn GmailApi,
    mount_id: &MountId,
    label_id: &str,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let list = api.list_messages(label_id, RECENT_LIMIT, None)?;
    list.messages
        .into_iter()
        .map(|message_ref| {
            let message = api.get_message_metadata(&message_ref.id)?;
            Ok(message_entry(
                mount_id,
                parent_path,
                mailbox_from_labels(&message.label_ids),
                message,
            ))
        })
        .collect()
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
            LocalityError::Io(format!("gmail draft frontmatter decode failed: {error}"))
        })?
    };
    if frontmatter.attachment.is_some() || frontmatter.attachments.is_some() {
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
    if properties.contains_key("attachments") || properties.contains_key("attachment") {
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

    use locality_connector::{ChildContainer, Connector, EnumerateRequest, ListChildrenRequest};
    use locality_core::journal::{PushId, PushOperationId};
    use locality_core::model::{EntityKind, MountId, RemoteId};
    use locality_core::planner::{PushOperation, PushPlan};
    use locality_core::push::RemotePrecondition;

    use super::{GmailConfig, GmailConnector};
    use crate::client::GmailApi;
    use crate::dto::{
        GmailDraft, GmailDraftCreateRequest, GmailDraftSendRequest, GmailMessage, GmailMessageList,
        GmailMessageRef,
    };

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
                properties: std::collections::BTreeMap::from([(
                    "to".to_string(),
                    locality_core::planner::PropertyValue::List(vec![
                        "ann@example.com".to_string(),
                    ]),
                )]),
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
            .expect("apply");

        assert_eq!(result.changed_remote_ids, vec![RemoteId::new("sent-msg-1")]);
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.created_drafts, 1);
        assert_eq!(calls.sent_drafts, vec!["draft-1"]);
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
        created_drafts: usize,
        sent_drafts: Vec<String>,
    }

    impl GmailApi for FakeGmailApi {
        fn list_messages(
            &self,
            label_id: &str,
            max_results: u32,
            _page_token: Option<&str>,
        ) -> locality_core::LocalityResult<GmailMessageList> {
            self.calls
                .lock()
                .expect("calls")
                .list_max_results
                .push(max_results);
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

        fn get_message_metadata(
            &self,
            message_id: &str,
        ) -> locality_core::LocalityResult<GmailMessage> {
            Ok(message_fixture(message_id))
        }

        fn get_message_full(
            &self,
            message_id: &str,
        ) -> locality_core::LocalityResult<GmailMessage> {
            Ok(message_fixture(message_id))
        }

        fn create_draft(
            &self,
            _request: GmailDraftCreateRequest,
        ) -> locality_core::LocalityResult<GmailDraft> {
            self.calls.lock().expect("calls").created_drafts += 1;
            Ok(GmailDraft {
                id: "draft-1".to_string(),
                message: message_fixture("draft-message-1"),
            })
        }

        fn send_draft(
            &self,
            request: GmailDraftSendRequest,
        ) -> locality_core::LocalityResult<GmailMessage> {
            self.calls
                .lock()
                .expect("calls")
                .sent_drafts
                .push(request.id);
            Ok(message_fixture("sent-msg-1"))
        }
    }

    fn message_fixture(id: &str) -> GmailMessage {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "threadId": format!("{id}-thread"),
            "labelIds": if id.starts_with("sent") { vec!["SENT"] } else { vec!["INBOX"] },
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
