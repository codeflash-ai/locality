use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, ChildContainer,
    Connector, ConnectorCapabilities, ConnectorExecutionPolicy, ConnectorKind, EnumerateRequest,
    FetchRequest, ListChildrenRequest, ListChildrenResult, NativeEntity, ObserveRequest,
    ParsedEntity,
};
use locality_core::freshness::{RemoteObservation, RemoteVersion};
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::PushOperationKind;
use locality_core::{LocalityError, LocalityResult};

use crate::client::{HttpSlackApiClient, SlackApi};
use crate::dto::{SlackConversation, SlackUser};
use crate::render::{
    SlackNativeBundle, SlackRenderedKind, conversation_remote_id, parse_recent_remote_id,
    recent_remote_id, render_slack_entity, slack_remote_version, users_remote_id,
};
use crate::settings::{SlackConversationType, SlackMountSettings};

pub const SLACK_CONNECTOR_ID: &str = "slack";
const CONVERSATIONS_PAGE_SIZE: u32 = 200;
const USERS_PAGE_SIZE: u32 = 200;

const CHANNELS_FOLDER_ID: &str = "slack-folder:channels";
const PRIVATE_CHANNELS_FOLDER_ID: &str = "slack-folder:private-channels";
const DMS_FOLDER_ID: &str = "slack-folder:dms";
const GROUP_DMS_FOLDER_ID: &str = "slack-folder:group-dms";

#[derive(Clone, PartialEq, Eq)]
pub struct SlackConfig {
    pub access_token: String,
    pub settings: SlackMountSettings,
    pub execution_policy: ConnectorExecutionPolicy,
}

impl SlackConfig {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            settings: SlackMountSettings::default(),
            execution_policy: ConnectorExecutionPolicy::Inline,
        }
    }

    pub fn with_settings(mut self, settings: SlackMountSettings) -> Self {
        self.settings = settings;
        self
    }

    pub fn with_execution_policy(mut self, execution_policy: ConnectorExecutionPolicy) -> Self {
        self.execution_policy = execution_policy;
        self
    }
}

impl fmt::Debug for SlackConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackConfig")
            .field("access_token", &"<redacted>")
            .field("settings", &self.settings)
            .field("execution_policy", &self.execution_policy)
            .finish()
    }
}

#[derive(Clone)]
pub struct SlackConnector {
    config: SlackConfig,
    api: Arc<dyn SlackApi>,
}

impl fmt::Debug for SlackConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackConnector")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl SlackConnector {
    pub fn new(config: SlackConfig) -> Self {
        let api = Arc::new(HttpSlackApiClient::with_execution_policy(
            config.access_token.clone(),
            config.execution_policy,
        ));
        Self::with_api(config, api)
    }

    pub fn with_api(config: SlackConfig, api: Arc<dyn SlackApi>) -> Self {
        Self { config, api }
    }

    pub fn config(&self) -> &SlackConfig {
        &self.config
    }

    fn all_conversations(&self) -> LocalityResult<Vec<SlackConversation>> {
        let types = self.config.settings.conversations_api_types();
        self.conversations_for_types(&types)
    }

    fn conversations_for_type(
        &self,
        conversation_type: &SlackConversationType,
    ) -> LocalityResult<Vec<SlackConversation>> {
        if !self.config.settings.slack.types.contains(conversation_type) {
            return Ok(Vec::new());
        }
        self.conversations_for_types(conversation_type.conversations_api_value())
    }

    fn conversations_for_types(&self, types: &str) -> LocalityResult<Vec<SlackConversation>> {
        let mut conversations = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = BTreeSet::new();

        loop {
            let page =
                self.api
                    .conversations_list(types, cursor.as_deref(), CONVERSATIONS_PAGE_SIZE)?;
            for conversation in page.channels {
                if let Some(conversation) = self.projectable_conversation(conversation)? {
                    conversations.push(conversation);
                }
            }

            let next_cursor = non_empty_cursor(page.response_metadata.next_cursor);
            let Some(next_cursor) = next_cursor else {
                break;
            };
            if !seen_cursors.insert(next_cursor.clone()) {
                return Err(LocalityError::InvalidState(format!(
                    "Slack conversations pagination returned repeated cursor `{next_cursor}`"
                )));
            }
            cursor = Some(next_cursor);
        }

        conversations.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(conversations)
    }

    fn all_users(&self) -> LocalityResult<Vec<SlackUser>> {
        let mut users = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = BTreeSet::new();

        loop {
            let page = self.api.users_list(cursor.as_deref(), USERS_PAGE_SIZE)?;
            users.extend(page.members);

            let next_cursor = non_empty_cursor(page.response_metadata.next_cursor);
            let Some(next_cursor) = next_cursor else {
                break;
            };
            if !seen_cursors.insert(next_cursor.clone()) {
                return Err(LocalityError::InvalidState(format!(
                    "Slack users pagination returned repeated cursor `{next_cursor}`"
                )));
            }
            cursor = Some(next_cursor);
        }

        users.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(users)
    }

    fn users_for_titles_if_needed(
        &self,
        conversations: &[SlackConversation],
    ) -> LocalityResult<BTreeMap<String, SlackUser>> {
        if conversations.iter().any(conversation_needs_user_title) {
            Ok(users_by_id(self.all_users()?))
        } else {
            Ok(BTreeMap::new())
        }
    }

    fn projectable_conversation(
        &self,
        conversation: SlackConversation,
    ) -> LocalityResult<Option<SlackConversation>> {
        if conversation.is_archived {
            return Ok(None);
        }
        if conversation_history_is_readable(&conversation) {
            return Ok(Some(conversation));
        }
        if self.config.settings.slack.auto_join_public_channels
            && public_channel_auto_join_is_allowed(&conversation)
        {
            return Ok(Some(self.join_public_channel(conversation)?));
        }
        Ok(None)
    }

    fn join_public_channel(
        &self,
        conversation: SlackConversation,
    ) -> LocalityResult<SlackConversation> {
        let channel = self.api.conversations_join(&conversation.id)?;
        Ok(channel.channel.unwrap_or(SlackConversation {
            is_member: Some(true),
            ..conversation
        }))
    }
}

impl Connector for SlackConnector {
    fn with_execution_policy(&self, policy: ConnectorExecutionPolicy) -> Self {
        Self::new(self.config.clone().with_execution_policy(policy))
    }

    fn kind(&self) -> ConnectorKind {
        ConnectorKind(SLACK_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_oauth: true,
            ..ConnectorCapabilities::read_only()
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        BTreeSet::new()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        let conversations = self.all_conversations()?;
        let users = self.all_users()?;
        let users_version = users_remote_version(&users)?;
        let users = if conversations.iter().any(conversation_needs_user_title) {
            users_by_id(users)
        } else {
            BTreeMap::new()
        };
        let mut entries = root_entries(&request.mount_id, Path::new(""), users_version);

        entries.extend(conversations.iter().map(|conversation| {
            let parent_path = Path::new(conversation_type(conversation).root_folder());
            conversation_entry(&request.mount_id, parent_path, conversation, &users)
        }));
        Ok(entries)
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let entries = match request.container {
            ChildContainer::Root => root_entries(
                &request.mount_id,
                &request.parent_path,
                users_remote_version(&self.all_users()?)?,
            ),
            ChildContainer::DirectoryChildren(remote_id) => {
                if let Some(folder_type) = folder_type_for_remote_id(remote_id.as_str()) {
                    let conversations = self.conversations_for_type(&folder_type)?;
                    let users = self.users_for_titles_if_needed(&conversations)?;
                    conversations
                        .iter()
                        .filter(|conversation| {
                            conversation_type(conversation).root_folder()
                                == folder_type.root_folder()
                        })
                        .map(|conversation| {
                            conversation_entry(
                                &request.mount_id,
                                &request.parent_path,
                                conversation,
                                &users,
                            )
                        })
                        .collect()
                } else if let Some(conversation_id) =
                    remote_id.as_str().strip_prefix("slack-conversation:")
                {
                    vec![recent_entry(
                        &request.mount_id,
                        &request.parent_path,
                        conversation_id,
                    )]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        };
        Ok(ListChildrenResult::complete(entries))
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        let remote_id_value = request.remote_id.as_str().to_string();
        if remote_id_value == users_remote_id() {
            let bundle = users_bundle(self.all_users()?);
            let version = slack_remote_version(&bundle)?;
            return Ok(RemoteObservation::new(
                request.mount_id,
                request.remote_id,
                EntityKind::Page,
                "users",
                "users.md",
            )
            .with_remote_version(RemoteVersion::new(version))
            .with_raw_metadata_json(serde_json::json!({ "kind": "slack_users" }).to_string()));
        }

        if let Some(conversation_id) = parse_recent_remote_id(&remote_id_value) {
            let conversation = self
                .all_conversations()?
                .into_iter()
                .find(|conversation| conversation.id == conversation_id)
                .ok_or_else(|| LocalityError::RemoteNotFound(conversation_id.to_string()))?;
            let users = self.all_users()?;
            let users_by_id = users_by_id(users.clone());
            let conversation_entry = conversation_entry(
                &request.mount_id,
                Path::new(conversation_type(&conversation).root_folder()),
                &conversation,
                &users_by_id,
            );
            let history = self.api.conversations_history(
                conversation_id,
                None,
                self.config.settings.slack.history_limit,
            )?;
            let bundle = recent_bundle(conversation, users, history.messages);
            let version = slack_remote_version(&bundle)?;
            return Ok(RemoteObservation::new(
                request.mount_id,
                request.remote_id,
                EntityKind::Page,
                "recent",
                conversation_entry.path.join("recent.md"),
            )
            .with_parent(conversation_entry.remote_id)
            .with_remote_version(RemoteVersion::new(version))
            .with_raw_metadata_json(
                serde_json::json!({
                    "kind": "slack_recent",
                    "conversation_id": conversation_id,
                })
                .to_string(),
            ));
        }

        Err(LocalityError::Unsupported(
            "Slack observation for this entity",
        ))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        if request.remote_id.as_str() == users_remote_id() {
            let bundle = users_bundle(self.all_users()?);
            return native_entity(request.remote_id, "slack_users", bundle);
        }

        let Some(conversation_id) = parse_recent_remote_id(request.remote_id.as_str()) else {
            return Err(LocalityError::Unsupported("Slack fetch for this entity"));
        };
        let conversation = self
            .all_conversations()?
            .into_iter()
            .find(|conversation| conversation.id == conversation_id)
            .ok_or_else(|| LocalityError::RemoteNotFound(conversation_id.to_string()))?;
        let history = self.api.conversations_history(
            conversation_id,
            None,
            self.config.settings.slack.history_limit,
        )?;
        let bundle = recent_bundle(conversation, self.all_users()?, history.messages);
        native_entity(request.remote_id, "slack_recent", bundle)
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        let bundle = serde_json::from_slice::<SlackNativeBundle>(&entity.raw)
            .map_err(|error| LocalityError::Io(format!("Slack native decode failed: {error}")))?;
        render_slack_entity(&bundle)
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::Unsupported("Slack writes"))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Err(LocalityError::Unsupported("Slack writes"))
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        Err(LocalityError::Unsupported("Slack writes"))
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::Unsupported("Slack writes"))
    }
}

fn native_entity(
    remote_id: RemoteId,
    kind: &'static str,
    bundle: SlackNativeBundle,
) -> LocalityResult<NativeEntity> {
    let raw = serde_json::to_vec(&bundle)
        .map_err(|error| LocalityError::Io(format!("Slack native encode failed: {error}")))?;
    Ok(NativeEntity {
        remote_id,
        kind: kind.to_string(),
        raw,
    })
}

fn users_bundle(users: Vec<SlackUser>) -> SlackNativeBundle {
    SlackNativeBundle {
        kind: SlackRenderedKind::Users,
        conversation: None,
        users,
        messages: Vec::new(),
    }
}

fn users_remote_version(users: &[SlackUser]) -> LocalityResult<String> {
    slack_remote_version(&users_bundle(users.to_vec()))
}

fn recent_bundle(
    conversation: SlackConversation,
    users: Vec<SlackUser>,
    messages: Vec<crate::dto::SlackMessage>,
) -> SlackNativeBundle {
    SlackNativeBundle {
        kind: SlackRenderedKind::Recent,
        conversation: Some(conversation),
        users,
        messages,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RootEntrySpec {
    remote_id: &'static str,
    title: &'static str,
    kind: EntityKind,
    path: &'static str,
}

fn root_specs() -> [RootEntrySpec; 5] {
    [
        RootEntrySpec {
            remote_id: CHANNELS_FOLDER_ID,
            title: "channels",
            kind: EntityKind::Directory,
            path: "channels",
        },
        RootEntrySpec {
            remote_id: PRIVATE_CHANNELS_FOLDER_ID,
            title: "private-channels",
            kind: EntityKind::Directory,
            path: "private-channels",
        },
        RootEntrySpec {
            remote_id: DMS_FOLDER_ID,
            title: "dms",
            kind: EntityKind::Directory,
            path: "dms",
        },
        RootEntrySpec {
            remote_id: GROUP_DMS_FOLDER_ID,
            title: "group-dms",
            kind: EntityKind::Directory,
            path: "group-dms",
        },
        RootEntrySpec {
            remote_id: "slack-users",
            title: "users",
            kind: EntityKind::Page,
            path: "users.md",
        },
    ]
}

fn root_entries(mount_id: &MountId, parent_path: &Path, users_version: String) -> Vec<TreeEntry> {
    root_specs()
        .into_iter()
        .map(|spec| TreeEntry {
            mount_id: mount_id.clone(),
            remote_id: RemoteId::new(spec.remote_id),
            kind: spec.kind,
            title: spec.title.to_string(),
            path: parent_path.join(spec.path),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: Some(root_entry_version(spec.remote_id, &users_version)),
            stub_frontmatter: None,
        })
        .collect()
}

fn root_entry_version(remote_id: &str, users_version: &str) -> String {
    if remote_id == users_remote_id() {
        users_version.to_string()
    } else {
        remote_id.to_string()
    }
}

fn folder_type_for_remote_id(remote_id: &str) -> Option<SlackConversationType> {
    match remote_id {
        CHANNELS_FOLDER_ID => Some(SlackConversationType::PublicChannel),
        PRIVATE_CHANNELS_FOLDER_ID => Some(SlackConversationType::PrivateChannel),
        DMS_FOLDER_ID => Some(SlackConversationType::Im),
        GROUP_DMS_FOLDER_ID => Some(SlackConversationType::Mpim),
        _ => None,
    }
}

fn conversation_entry(
    mount_id: &MountId,
    parent_path: &Path,
    conversation: &SlackConversation,
    users: &BTreeMap<String, SlackUser>,
) -> TreeEntry {
    let title = conversation_title(conversation, users);
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(conversation_remote_id(&conversation.id)),
        kind: EntityKind::Directory,
        title: title.clone(),
        path: parent_path.join(conversation_directory_name(&title, &conversation.id)),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(conversation_version(conversation)),
        stub_frontmatter: None,
    }
}

fn recent_entry(mount_id: &MountId, parent_path: &Path, conversation_id: &str) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(recent_remote_id(conversation_id)),
        kind: EntityKind::Page,
        title: "recent".to_string(),
        path: parent_path.join("recent.md"),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: None,
        stub_frontmatter: None,
    }
}

fn conversation_type(conversation: &SlackConversation) -> SlackConversationType {
    if conversation.is_im {
        SlackConversationType::Im
    } else if conversation.is_mpim {
        SlackConversationType::Mpim
    } else if conversation.is_group || conversation.is_private {
        SlackConversationType::PrivateChannel
    } else {
        SlackConversationType::PublicChannel
    }
}

fn conversation_history_is_readable(conversation: &SlackConversation) -> bool {
    !(conversation.is_channel && conversation.is_member == Some(false))
}

fn public_channel_auto_join_is_allowed(conversation: &SlackConversation) -> bool {
    conversation.is_channel
        && !conversation.is_private
        && !conversation.is_group
        && !conversation.is_im
        && !conversation.is_mpim
}

fn conversation_title(
    conversation: &SlackConversation,
    users: &BTreeMap<String, SlackUser>,
) -> String {
    conversation
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            conversation
                .user
                .as_ref()
                .and_then(|user_id| users.get(user_id))
                .map(user_display_name)
        })
        .unwrap_or_else(|| conversation.id.clone())
}

fn conversation_needs_user_title(conversation: &SlackConversation) -> bool {
    conversation
        .name
        .as_deref()
        .is_none_or(|name| name.trim().is_empty())
        && conversation.user.is_some()
}

fn user_display_name(user: &SlackUser) -> String {
    user.profile
        .as_ref()
        .and_then(|profile| profile.real_name.as_deref())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            user.profile
                .as_ref()
                .and_then(|profile| profile.display_name.as_deref())
        })
        .filter(|value| !value.trim().is_empty())
        .or(user.real_name.as_deref())
        .filter(|value| !value.trim().is_empty())
        .or(user.name.as_deref())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(user.id.as_str())
        .to_string()
}

fn conversation_directory_name(title: &str, conversation_id: &str) -> String {
    format!("{}-{}", safe_segment(title), safe_segment(conversation_id))
}

fn safe_segment(value: &str) -> String {
    let segment = value
        .chars()
        .map(|character| match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            other if other.is_control() => '-',
            other => other,
        })
        .collect::<String>();
    if segment.trim().is_empty() || segment == "." || segment == ".." {
        "untitled".to_string()
    } else {
        segment
    }
}

fn users_by_id(users: Vec<SlackUser>) -> BTreeMap<String, SlackUser> {
    users
        .into_iter()
        .map(|user| (user.id.clone(), user))
        .collect()
}

fn conversation_version(conversation: &SlackConversation) -> String {
    match conversation.updated {
        Some(updated) => format!("slack:{}:{updated}", conversation.id),
        None => format!("slack:{}:", conversation.id),
    }
}

fn non_empty_cursor(cursor: Option<String>) -> Option<String> {
    cursor.filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::SlackApi;
    use crate::dto::{
        SlackAuthTestResponse, SlackConversation, SlackConversationsListResponse,
        SlackHistoryResponse, SlackJoinResponse, SlackMessage, SlackResponseMetadata, SlackUser,
        SlackUsersListResponse,
    };
    use crate::render::SlackNativeBundle;
    use locality_connector::{
        ChildContainer, Connector, FetchRequest, ListChildrenRequest, ObserveRequest,
    };
    use locality_core::model::{CanonicalDocument, EntityKind, MountId, RemoteId};
    use locality_core::{LocalityError, LocalityResult};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[test]
    fn root_lists_fixed_read_only_directories_and_users() {
        let connector = connector_with_api(FakeSlackApi::default());
        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("slack-main"),
                container: ChildContainer::Root,
                parent_path: PathBuf::new(),
            })
            .expect("list root");

        let paths = result
            .entries
            .iter()
            .map(|entry| entry.path.display().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                "channels",
                "private-channels",
                "dms",
                "group-dms",
                "users.md"
            ]
        );
        assert!(result.is_complete());
        assert!(result.entries.iter().all(|entry| {
            entry.hydration == locality_core::model::HydrationState::Stub
                && entry.content_hash.is_none()
        }));
        assert_eq!(result.entries[4].kind, EntityKind::Page);
    }

    #[test]
    fn channel_folder_lists_conversations_from_api() {
        let api = FakeSlackApi::default().with_conversations(vec![
            SlackConversation {
                id: "C_archived".to_string(),
                name: Some("old".to_string()),
                is_channel: true,
                is_archived: true,
                ..SlackConversation::default()
            },
            SlackConversation {
                id: "C123".to_string(),
                name: Some("general".to_string()),
                is_channel: true,
                ..SlackConversation::default()
            },
            SlackConversation {
                id: "D123".to_string(),
                user: Some("U123".to_string()),
                is_im: true,
                ..SlackConversation::default()
            },
        ]);
        let connector = connector_with_api(api);

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("slack-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new(
                    "slack-folder:channels",
                )),
                parent_path: PathBuf::from("channels"),
            })
            .expect("list channels");

        assert_eq!(result.entries.len(), 1);
        assert_eq!(
            result.entries[0].remote_id.as_str(),
            "slack-conversation:C123"
        );
        assert_eq!(result.entries[0].kind, EntityKind::Directory);
        assert_eq!(
            result.entries[0].path,
            PathBuf::from("channels/general-C123")
        );
        assert!(result.is_complete());
    }

    #[test]
    fn channel_folder_auto_joins_public_channels_where_bot_is_not_a_member() {
        let api = FakeSlackApi::default().with_conversations(vec![
            SlackConversation {
                id: "C_unjoined".to_string(),
                name: Some("unjoined".to_string()),
                is_channel: true,
                is_member: Some(false),
                ..SlackConversation::default()
            },
            SlackConversation {
                id: "C_joined".to_string(),
                name: Some("joined".to_string()),
                is_channel: true,
                is_member: Some(true),
                ..SlackConversation::default()
            },
        ]);
        let connector = connector_with_api(api.clone());

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("slack-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new(
                    "slack-folder:channels",
                )),
                parent_path: PathBuf::from("channels"),
            })
            .expect("list channels");

        let paths = result
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            *api.joined_channels.lock().expect("joined channels"),
            vec!["C_unjoined".to_string()]
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from("channels/joined-C_joined"),
                PathBuf::from("channels/unjoined-C_unjoined")
            ]
        );
    }

    #[test]
    fn auto_join_public_channels_joins_before_projecting_unjoined_public_channels() {
        let api = FakeSlackApi::default().with_conversations(vec![SlackConversation {
            id: "C_unjoined".to_string(),
            name: Some("unjoined".to_string()),
            is_channel: true,
            is_member: Some(false),
            ..SlackConversation::default()
        }]);
        let settings = SlackMountSettings::from_json(r#"{"slack":{"types":["public_channel"]}}"#)
            .expect("settings");
        let connector = SlackConnector::with_api(
            SlackConfig::new("xoxb-token").with_settings(settings),
            Arc::new(api.clone()),
        );

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("slack-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new(
                    "slack-folder:channels",
                )),
                parent_path: PathBuf::from("channels"),
            })
            .expect("list channels");

        assert_eq!(
            *api.joined_channels.lock().expect("joined channels"),
            vec!["C_unjoined".to_string()]
        );
        assert_eq!(result.entries.len(), 1);
        assert_eq!(
            result.entries[0].path,
            PathBuf::from("channels/unjoined-C_unjoined")
        );
    }

    #[test]
    fn auto_join_public_channels_skips_unjoined_private_channels() {
        let api = FakeSlackApi::default().with_conversations(vec![SlackConversation {
            id: "G_private".to_string(),
            name: Some("private".to_string()),
            is_channel: true,
            is_private: true,
            is_member: Some(false),
            ..SlackConversation::default()
        }]);
        let settings = SlackMountSettings::from_json(r#"{"slack":{"types":["private_channel"]}}"#)
            .expect("settings");
        let connector = SlackConnector::with_api(
            SlackConfig::new("xoxb-token").with_settings(settings),
            Arc::new(api.clone()),
        );

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("slack-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new(
                    "slack-folder:private-channels",
                )),
                parent_path: PathBuf::from("private-channels"),
            })
            .expect("list private channels");

        assert!(
            api.joined_channels
                .lock()
                .expect("joined channels")
                .is_empty()
        );
        assert!(result.entries.is_empty());
    }

    #[test]
    fn conversation_directory_lists_recent_markdown() {
        let connector = connector_with_api(FakeSlackApi::default());
        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("slack-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new(
                    "slack-conversation:C123",
                )),
                parent_path: PathBuf::from("channels/general-C123"),
            })
            .expect("list conversation");

        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].remote_id.as_str(), "slack-recent:C123");
        assert_eq!(result.entries[0].kind, EntityKind::Page);
        assert_eq!(
            result.entries[0].path,
            PathBuf::from("channels/general-C123/recent.md")
        );
        assert_eq!(result.entries[0].remote_edited_at, None);
        assert!(result.is_complete());
    }

    #[test]
    fn fetch_recent_uses_bounded_history_limit() {
        let api = FakeSlackApi::default()
            .with_conversations(vec![SlackConversation {
                id: "C123".to_string(),
                name: Some("general".to_string()),
                is_channel: true,
                ..SlackConversation::default()
            }])
            .with_messages(vec![SlackMessage {
                user: Some("U123".to_string()),
                text: "hello".to_string(),
                ts: "1780000000.000100".to_string(),
                ..SlackMessage::default()
            }]);
        let connector = connector_with_api(api.clone());

        let native = connector
            .fetch(FetchRequest {
                remote_id: RemoteId::new("slack-recent:C123"),
            })
            .expect("fetch recent");

        assert_eq!(native.kind, "slack_recent");
        assert_eq!(*api.history_limit.lock().expect("history limit"), Some(15));
        let bundle: SlackNativeBundle = serde_json::from_slice(&native.raw).expect("bundle");
        assert_eq!(bundle.conversation.expect("conversation").id, "C123");
        assert_eq!(bundle.messages.len(), 1);

        let observation = connector
            .observe(ObserveRequest {
                mount_id: MountId::new("slack-main"),
                remote_id: RemoteId::new("slack-recent:C123"),
            })
            .expect("observe recent");
        assert_content_remote_version(observation.remote_version.expect("version").as_str());
    }

    #[test]
    fn users_fetch_render_and_observe_versions_match_renderer_frontmatter() {
        let connector = connector_with_api(FakeSlackApi::default());

        let native = connector
            .fetch(FetchRequest {
                remote_id: RemoteId::new("slack-users"),
            })
            .expect("fetch users");
        let document = connector.render(&native).expect("render users");
        let rendered_version = remote_edited_at_from_frontmatter(&document.frontmatter);
        let observation = connector
            .observe(ObserveRequest {
                mount_id: MountId::new("slack-main"),
                remote_id: RemoteId::new("slack-users"),
            })
            .expect("observe users");
        let root = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("slack-main"),
                container: ChildContainer::Root,
                parent_path: PathBuf::new(),
            })
            .expect("list root");
        let users_entry = root
            .entries
            .iter()
            .find(|entry| entry.remote_id.as_str() == "slack-users")
            .expect("users entry");

        assert_content_remote_version(rendered_version);
        assert_eq!(
            observation.remote_version.expect("version").as_str(),
            rendered_version
        );
        assert_eq!(
            users_entry.remote_edited_at.as_deref(),
            Some(rendered_version)
        );
    }

    #[test]
    fn recent_fetch_render_and_observe_versions_match_renderer_frontmatter_and_path() {
        let connector = connector_with_api(
            FakeSlackApi::default()
                .with_conversations(vec![SlackConversation {
                    id: "C123".to_string(),
                    name: Some("general".to_string()),
                    is_channel: true,
                    ..SlackConversation::default()
                }])
                .with_messages(vec![
                    SlackMessage {
                        text: "older".to_string(),
                        ts: "1780000000.000100".to_string(),
                        ..SlackMessage::default()
                    },
                    SlackMessage {
                        text: "newer".to_string(),
                        ts: "1780000001.000200".to_string(),
                        ..SlackMessage::default()
                    },
                ]),
        );

        let native = connector
            .fetch(FetchRequest {
                remote_id: RemoteId::new("slack-recent:C123"),
            })
            .expect("fetch recent");
        let document = connector.render(&native).expect("render recent");
        let rendered_version = remote_edited_at_from_frontmatter(&document.frontmatter);
        let observation = connector
            .observe(ObserveRequest {
                mount_id: MountId::new("slack-main"),
                remote_id: RemoteId::new("slack-recent:C123"),
            })
            .expect("observe recent");

        assert_content_remote_version(rendered_version);
        assert_eq!(
            observation.remote_version.expect("version").as_str(),
            rendered_version
        );
        assert_eq!(
            observation.projected_path,
            PathBuf::from("channels/general-C123/recent.md")
        );
    }

    #[test]
    fn remote_version_for_users_changes_when_profile_display_changes() {
        let before = observe_version(
            connector_with_api(FakeSlackApi::default().with_users(vec![SlackUser {
                id: "U123".to_string(),
                name: Some("ada".to_string()),
                profile: Some(crate::dto::SlackUserProfile {
                    display_name: Some("Ada".to_string()),
                    ..crate::dto::SlackUserProfile::default()
                }),
                ..SlackUser::default()
            }])),
            "slack-users",
        );
        let after = observe_version(
            connector_with_api(FakeSlackApi::default().with_users(vec![SlackUser {
                id: "U123".to_string(),
                name: Some("ada".to_string()),
                profile: Some(crate::dto::SlackUserProfile {
                    display_name: Some("Ada Byron".to_string()),
                    ..crate::dto::SlackUserProfile::default()
                }),
                ..SlackUser::default()
            }])),
            "slack-users",
        );

        assert_content_remote_version(&before);
        assert_content_remote_version(&after);
        assert_ne!(before, after);
    }

    #[test]
    fn remote_version_for_recent_changes_when_message_text_changes_without_latest_timestamp_change()
    {
        let before = observe_version(
            connector_with_api(
                FakeSlackApi::default()
                    .with_conversations(vec![SlackConversation {
                        id: "C123".to_string(),
                        name: Some("general".to_string()),
                        is_channel: true,
                        ..SlackConversation::default()
                    }])
                    .with_messages(vec![SlackMessage {
                        text: "original message".to_string(),
                        ts: "1780000000.000100".to_string(),
                        ..SlackMessage::default()
                    }]),
            ),
            "slack-recent:C123",
        );
        let after = observe_version(
            connector_with_api(
                FakeSlackApi::default()
                    .with_conversations(vec![SlackConversation {
                        id: "C123".to_string(),
                        name: Some("general".to_string()),
                        is_channel: true,
                        ..SlackConversation::default()
                    }])
                    .with_messages(vec![SlackMessage {
                        text: "edited message".to_string(),
                        ts: "1780000000.000100".to_string(),
                        ..SlackMessage::default()
                    }]),
            ),
            "slack-recent:C123",
        );

        assert_content_remote_version(&before);
        assert_content_remote_version(&after);
        assert_ne!(before, after);
    }

    #[test]
    fn remote_version_for_recent_changes_when_rendered_user_display_changes() {
        let before = observe_version(
            connector_with_api(
                FakeSlackApi::default()
                    .with_conversations(vec![SlackConversation {
                        id: "C123".to_string(),
                        name: Some("general".to_string()),
                        is_channel: true,
                        ..SlackConversation::default()
                    }])
                    .with_users(vec![SlackUser {
                        id: "U123".to_string(),
                        name: Some("ada".to_string()),
                        profile: Some(crate::dto::SlackUserProfile {
                            display_name: Some("Ada".to_string()),
                            ..crate::dto::SlackUserProfile::default()
                        }),
                        ..SlackUser::default()
                    }])
                    .with_messages(vec![SlackMessage {
                        user: Some("U123".to_string()),
                        text: "hello <@U123>".to_string(),
                        ts: "1780000000.000100".to_string(),
                        ..SlackMessage::default()
                    }]),
            ),
            "slack-recent:C123",
        );
        let after = observe_version(
            connector_with_api(
                FakeSlackApi::default()
                    .with_conversations(vec![SlackConversation {
                        id: "C123".to_string(),
                        name: Some("general".to_string()),
                        is_channel: true,
                        ..SlackConversation::default()
                    }])
                    .with_users(vec![SlackUser {
                        id: "U123".to_string(),
                        name: Some("ada".to_string()),
                        profile: Some(crate::dto::SlackUserProfile {
                            display_name: Some("Ada Byron".to_string()),
                            ..crate::dto::SlackUserProfile::default()
                        }),
                        ..SlackUser::default()
                    }])
                    .with_messages(vec![SlackMessage {
                        user: Some("U123".to_string()),
                        text: "hello <@U123>".to_string(),
                        ts: "1780000000.000100".to_string(),
                        ..SlackMessage::default()
                    }]),
            ),
            "slack-recent:C123",
        );

        assert_content_remote_version(&before);
        assert_content_remote_version(&after);
        assert_ne!(before, after);
    }

    #[test]
    fn observe_recent_returns_remote_not_found_for_absent_conversation() {
        let connector = connector_with_api(FakeSlackApi::default());

        let error = connector
            .observe(ObserveRequest {
                mount_id: MountId::new("slack-main"),
                remote_id: RemoteId::new("slack-recent:C404"),
            })
            .expect_err("absent conversation rejected");

        assert!(matches!(error, LocalityError::RemoteNotFound(message) if message == "C404"));
    }

    #[test]
    fn duplicate_dm_display_names_get_distinct_paths() {
        let connector = connector_with_api(
            FakeSlackApi::default()
                .with_conversations(vec![
                    SlackConversation {
                        id: "D123".to_string(),
                        user: Some("U123".to_string()),
                        is_im: true,
                        ..SlackConversation::default()
                    },
                    SlackConversation {
                        id: "D456".to_string(),
                        user: Some("U456".to_string()),
                        is_im: true,
                        ..SlackConversation::default()
                    },
                ])
                .with_users(vec![
                    SlackUser {
                        id: "U123".to_string(),
                        real_name: Some("Ada Lovelace".to_string()),
                        ..SlackUser::default()
                    },
                    SlackUser {
                        id: "U456".to_string(),
                        real_name: Some("Ada Lovelace".to_string()),
                        ..SlackUser::default()
                    },
                ]),
        );

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("slack-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new("slack-folder:dms")),
                parent_path: PathBuf::from("dms"),
            })
            .expect("list dms");
        let paths = result
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            vec![
                PathBuf::from("dms/Ada Lovelace-D123"),
                PathBuf::from("dms/Ada Lovelace-D456")
            ]
        );
    }

    #[test]
    fn channel_listing_uses_public_channel_type_and_skips_users_for_named_channels() {
        let api = FakeSlackApi::default().with_conversations(vec![SlackConversation {
            id: "C123".to_string(),
            name: Some("general".to_string()),
            is_channel: true,
            ..SlackConversation::default()
        }]);
        let connector = connector_with_api(api.clone());

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("slack-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new(
                    "slack-folder:channels",
                )),
                parent_path: PathBuf::from("channels"),
            })
            .expect("list channels");

        assert_eq!(result.entries.len(), 1);
        assert_eq!(
            api.conversation_types.lock().expect("types").as_slice(),
            &["public_channel".to_string()]
        );
        assert_eq!(*api.users_calls.lock().expect("users calls"), 0);
    }

    #[test]
    fn excluded_folder_returns_empty_without_conversations_call() {
        let api = FakeSlackApi::default();
        let settings =
            SlackMountSettings::from_json(r#"{"slack":{"types":["im"]}}"#).expect("settings");
        let connector = SlackConnector::with_api(
            SlackConfig::new("xoxb-token").with_settings(settings),
            Arc::new(api.clone()),
        );

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("slack-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new(
                    "slack-folder:channels",
                )),
                parent_path: PathBuf::from("channels"),
            })
            .expect("list excluded channels");

        assert!(result.entries.is_empty());
        assert!(result.is_complete());
        assert!(api.conversation_types.lock().expect("types").is_empty());
        assert_eq!(*api.users_calls.lock().expect("users calls"), 0);
    }

    #[test]
    fn writes_are_unsupported() {
        let connector = connector_with_api(FakeSlackApi::default());

        assert_eq!(connector.kind().0, SLACK_CONNECTOR_ID);
        let expected_capabilities = ConnectorCapabilities {
            supports_oauth: true,
            ..ConnectorCapabilities::read_only()
        };
        assert_eq!(connector.capabilities(), expected_capabilities);
        assert!(connector.supported_push_operations().is_empty());
        assert!(
            connector
                .parse(&CanonicalDocument::new(String::new(), String::new()))
                .is_err()
        );
    }

    fn connector_with_api(api: FakeSlackApi) -> SlackConnector {
        SlackConnector::with_api(SlackConfig::new("xoxb-token"), Arc::new(api))
    }

    fn observe_version(connector: SlackConnector, remote_id: &str) -> String {
        connector
            .observe(ObserveRequest {
                mount_id: MountId::new("slack-main"),
                remote_id: RemoteId::new(remote_id),
            })
            .expect("observe")
            .remote_version
            .expect("remote version")
            .as_str()
            .to_string()
    }

    fn assert_content_remote_version(version: &str) {
        let hash = version
            .strip_prefix("content:")
            .unwrap_or_else(|| panic!("expected content version, got `{version}`"));
        assert!(!hash.is_empty(), "expected hash in `{version}`");
        assert!(
            hash.chars().all(|character| character.is_ascii_hexdigit()),
            "expected hex hash in `{version}`"
        );
    }

    #[derive(Clone, Debug, Default)]
    struct FakeSlackApi {
        conversations: Arc<Mutex<Vec<SlackConversation>>>,
        users: Arc<Mutex<Vec<SlackUser>>>,
        messages: Arc<Mutex<Vec<SlackMessage>>>,
        history_limit: Arc<Mutex<Option<u32>>>,
        conversation_types: Arc<Mutex<Vec<String>>>,
        joined_channels: Arc<Mutex<Vec<String>>>,
        users_calls: Arc<Mutex<usize>>,
    }

    impl FakeSlackApi {
        fn with_conversations(self, conversations: Vec<SlackConversation>) -> Self {
            *self.conversations.lock().expect("conversations") = conversations;
            self
        }

        fn with_users(self, users: Vec<SlackUser>) -> Self {
            *self.users.lock().expect("users") = users;
            self
        }

        fn with_messages(self, messages: Vec<SlackMessage>) -> Self {
            *self.messages.lock().expect("messages") = messages;
            self
        }
    }

    impl SlackApi for FakeSlackApi {
        fn auth_test(&self) -> LocalityResult<SlackAuthTestResponse> {
            Ok(SlackAuthTestResponse {
                ok: true,
                team_id: Some("T123".to_string()),
                team: Some("Locality".to_string()),
                ..SlackAuthTestResponse::default()
            })
        }

        fn conversations_list(
            &self,
            types: &str,
            _cursor: Option<&str>,
            _limit: u32,
        ) -> LocalityResult<SlackConversationsListResponse> {
            self.conversation_types
                .lock()
                .expect("conversation types")
                .push(types.to_string());
            Ok(SlackConversationsListResponse {
                ok: true,
                channels: self.conversations.lock().expect("conversations").clone(),
                response_metadata: SlackResponseMetadata::default(),
                error: None,
            })
        }

        fn conversations_history(
            &self,
            _channel: &str,
            _cursor: Option<&str>,
            limit: u32,
        ) -> LocalityResult<SlackHistoryResponse> {
            *self.history_limit.lock().expect("history limit") = Some(limit);
            Ok(SlackHistoryResponse {
                ok: true,
                messages: self.messages.lock().expect("messages").clone(),
                ..SlackHistoryResponse::default()
            })
        }

        fn conversations_join(&self, channel: &str) -> LocalityResult<SlackJoinResponse> {
            self.joined_channels
                .lock()
                .expect("joined channels")
                .push(channel.to_string());
            let joined = self
                .conversations
                .lock()
                .expect("conversations")
                .iter()
                .find(|conversation| conversation.id == channel)
                .cloned()
                .map(|mut conversation| {
                    conversation.is_member = Some(true);
                    conversation
                });
            Ok(SlackJoinResponse {
                ok: true,
                channel: joined,
                ..SlackJoinResponse::default()
            })
        }

        fn users_list(
            &self,
            _cursor: Option<&str>,
            _limit: u32,
        ) -> LocalityResult<SlackUsersListResponse> {
            *self.users_calls.lock().expect("users calls") += 1;
            let members = {
                let users = self.users.lock().expect("users");
                if users.is_empty() {
                    vec![SlackUser {
                        id: "U123".to_string(),
                        name: Some("ada".to_string()),
                        real_name: Some("Ada Lovelace".to_string()),
                        ..SlackUser::default()
                    }]
                } else {
                    users.clone()
                }
            };
            Ok(SlackUsersListResponse {
                ok: true,
                members,
                ..SlackUsersListResponse::default()
            })
        }
    }

    fn remote_edited_at_from_frontmatter(frontmatter: &str) -> &str {
        frontmatter
            .lines()
            .find_map(|line| line.trim().strip_prefix("remote_edited_at: "))
            .expect("remote_edited_at")
            .trim_matches('"')
    }
}
