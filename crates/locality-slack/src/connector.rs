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
use crate::dto::{SlackConversation, SlackMessage, SlackUser};
use crate::oauth::SLACK_CONNECTOR_ID;
use crate::render::{
    SlackContentKind, SlackNativeBundle, parse_slack_channel_remote_id,
    parse_slack_recent_remote_id, parse_slack_thread_remote_id, parse_slack_threads_remote_id,
    render_slack_document, safe_filename, slack_channel_remote_id, slack_recent_remote_id,
    slack_recent_version, slack_thread_remote_id, slack_thread_version, slack_threads_remote_id,
    thread_file_name,
};
use crate::settings::SlackMountSettings;

const CHANNELS_ROOT_ID: &str = "slack-dir:channels";
const SLACK_READ_ONLY_REASON: &str = "Slack connector is read-only in v1";

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
            .field("access_token", &"<redacted>")
            .finish()
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

    pub fn api(&self) -> &dyn SlackApi {
        self.api.as_ref()
    }

    fn all_channels(&self) -> LocalityResult<Vec<SlackConversation>> {
        let mut channels = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .api
                .list_public_channels(self.config.settings.recent_limit, cursor.as_deref())?;
            channels.extend(
                page.channels
                    .into_iter()
                    .filter(|channel| channel.is_channel && !channel.is_archived),
            );
            cursor = page.next_cursor.filter(|value| !value.is_empty());
            if cursor.is_none() {
                break;
            }
        }
        channels.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(channels)
    }

    fn channel_by_id(&self, channel_id: &str) -> LocalityResult<SlackConversation> {
        let channel = self.api.conversation_info(channel_id)?;
        if channel.is_archived {
            return Err(LocalityError::RemoteNotFound(format!(
                "Slack channel `{channel_id}`"
            )));
        }
        Ok(channel)
    }

    fn recent_history(&self, channel_id: &str) -> LocalityResult<Vec<SlackMessage>> {
        let mut messages = Vec::new();
        let mut cursor = None;
        while messages.len() < self.config.settings.recent_limit as usize {
            let remaining = self.config.settings.recent_limit as usize - messages.len();
            let limit = remaining.min(self.config.settings.recent_limit as usize) as u32;
            let page = self
                .api
                .conversation_history(channel_id, limit, cursor.as_deref())?;
            messages.extend(page.messages);
            cursor = page.next_cursor.filter(|value| !value.is_empty());
            if cursor.is_none() {
                break;
            }
        }
        messages.truncate(self.config.settings.recent_limit as usize);
        Ok(messages)
    }

    fn thread_history(
        &self,
        channel_id: &str,
        thread_ts: &str,
    ) -> LocalityResult<Vec<SlackMessage>> {
        let mut messages = Vec::new();
        let mut cursor = None;
        loop {
            let page = self.api.conversation_replies(
                channel_id,
                thread_ts,
                self.config.settings.recent_limit,
                cursor.as_deref(),
            )?;
            messages.extend(page.messages);
            cursor = page.next_cursor.filter(|value| !value.is_empty());
            if cursor.is_none() {
                break;
            }
        }
        Ok(messages)
    }

    fn users_for_messages(
        &self,
        messages: &[SlackMessage],
    ) -> LocalityResult<BTreeMap<String, SlackUser>> {
        let mut users = BTreeMap::new();
        for user_id in messages
            .iter()
            .filter_map(|message| message.user.as_deref())
        {
            if users.contains_key(user_id) {
                continue;
            }
            let user = self.api.user_info(user_id)?.user;
            users.insert(user_id.to_string(), user);
        }
        Ok(users)
    }

    fn bundle(
        &self,
        content_kind: SlackContentKind,
        channel: SlackConversation,
        thread_ts: Option<String>,
        messages: Vec<SlackMessage>,
    ) -> LocalityResult<SlackNativeBundle> {
        let users = self.users_for_messages(&messages)?;
        Ok(SlackNativeBundle {
            content_kind,
            channel_id: channel.id,
            channel_name: channel.name,
            recent_limit: self.config.settings.recent_limit,
            thread_ts,
            messages,
            users,
        })
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
        let mut entries = vec![channels_root_entry(&request.mount_id, Path::new(""))];
        entries.extend(
            self.all_channels()?
                .into_iter()
                .map(|channel| channel_entry(&request.mount_id, Path::new("channels"), &channel)),
        );
        Ok(entries)
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let entries = match request.container {
            ChildContainer::Root => {
                vec![channels_root_entry(&request.mount_id, &request.parent_path)]
            }
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == CHANNELS_ROOT_ID =>
            {
                self.all_channels()?
                    .into_iter()
                    .map(|channel| channel_entry(&request.mount_id, &request.parent_path, &channel))
                    .collect()
            }
            ChildContainer::DirectoryChildren(remote_id) => {
                if let Some(channel_id) = parse_slack_channel_remote_id(&remote_id) {
                    channel_stub_entries(&request.mount_id, &request.parent_path, channel_id)
                } else if let Some(channel_id) = parse_slack_threads_remote_id(&remote_id) {
                    let messages = self.recent_history(channel_id)?;
                    thread_stub_entries(
                        &request.mount_id,
                        &request.parent_path,
                        channel_id,
                        &messages,
                    )
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        };
        Ok(ListChildrenResult::complete(entries))
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        if request.remote_id.as_str() == CHANNELS_ROOT_ID {
            return Ok(observation_from_entry(
                channels_root_entry(&request.mount_id, Path::new("")),
                None,
            ));
        }
        if let Some(channel_id) = parse_slack_channel_remote_id(&request.remote_id) {
            let channel = self.channel_by_id(channel_id)?;
            return Ok(observation_from_entry(
                channel_entry(&request.mount_id, Path::new("channels"), &channel),
                Some(RemoteId::new(CHANNELS_ROOT_ID)),
            ));
        }
        if let Some(channel_id) = parse_slack_recent_remote_id(&request.remote_id) {
            let channel = self.channel_by_id(channel_id)?;
            let messages = self.recent_history(channel_id)?;
            let entry = recent_entry(
                &request.mount_id,
                &Path::new("channels").join(channel_directory_name(&channel)),
                channel_id,
            );
            let entry =
                entry_with_remote_version(entry, slack_recent_version(channel_id, &messages));
            return Ok(observation_from_entry(
                entry,
                Some(slack_channel_remote_id(channel_id)),
            ));
        }
        if let Some((channel_id, thread_ts)) = parse_slack_thread_remote_id(&request.remote_id) {
            let channel = self.channel_by_id(channel_id)?;
            let messages = self.thread_history(channel_id, thread_ts)?;
            let parent = Path::new("channels")
                .join(channel_directory_name(&channel))
                .join("threads");
            let entry = thread_entry(&request.mount_id, &parent, channel_id, thread_ts);
            let entry = entry_with_remote_version(
                entry,
                slack_thread_version(channel_id, thread_ts, &messages),
            );
            return Ok(observation_from_entry(
                entry,
                Some(slack_threads_remote_id(channel_id)),
            ));
        }
        Err(LocalityError::RemoteNotFound(request.remote_id.0))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        if let Some(channel_id) = parse_slack_recent_remote_id(&request.remote_id) {
            let channel = self.channel_by_id(channel_id)?;
            let messages = self.recent_history(channel_id)?;
            let bundle = self.bundle(SlackContentKind::Recent, channel, None, messages)?;
            let raw = serde_json::to_vec(&bundle).map_err(|error| {
                LocalityError::Io(format!("Slack native encode failed: {error}"))
            })?;
            return Ok(NativeEntity {
                remote_id: request.remote_id,
                kind: "slack_recent".to_string(),
                raw,
            });
        }
        if let Some((channel_id, thread_ts)) = parse_slack_thread_remote_id(&request.remote_id) {
            let channel = self.channel_by_id(channel_id)?;
            let messages = self.thread_history(channel_id, thread_ts)?;
            let bundle = self.bundle(
                SlackContentKind::Thread,
                channel,
                Some(thread_ts.to_string()),
                messages,
            )?;
            let raw = serde_json::to_vec(&bundle).map_err(|error| {
                LocalityError::Io(format!("Slack native encode failed: {error}"))
            })?;
            return Ok(NativeEntity {
                remote_id: request.remote_id,
                kind: "slack_thread".to_string(),
                raw,
            });
        }
        Err(LocalityError::Unsupported("Slack directory hydration"))
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        let bundle = serde_json::from_slice::<SlackNativeBundle>(&entity.raw)
            .map_err(|error| LocalityError::Io(format!("Slack native decode failed: {error}")))?;
        render_slack_document(&bundle)
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::Unsupported(SLACK_READ_ONLY_REASON))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Err(LocalityError::Unsupported(SLACK_READ_ONLY_REASON))
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        Err(LocalityError::Unsupported(SLACK_READ_ONLY_REASON))
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::Unsupported(SLACK_READ_ONLY_REASON))
    }
}

fn channels_root_entry(mount_id: &MountId, parent: &Path) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(CHANNELS_ROOT_ID),
        kind: EntityKind::Directory,
        title: "Slack channels".to_string(),
        path: parent.join("channels"),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: None,
        stub_frontmatter: None,
    }
}

fn channel_entry(mount_id: &MountId, parent: &Path, channel: &SlackConversation) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: slack_channel_remote_id(&channel.id),
        kind: EntityKind::Directory,
        title: format!("#{}", channel.name),
        path: parent.join(channel_directory_name(channel)),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: None,
        stub_frontmatter: None,
    }
}

fn channel_stub_entries(mount_id: &MountId, parent: &Path, channel_id: &str) -> Vec<TreeEntry> {
    vec![
        recent_entry(mount_id, parent, channel_id),
        TreeEntry {
            mount_id: mount_id.clone(),
            remote_id: slack_threads_remote_id(channel_id),
            kind: EntityKind::Directory,
            title: "Threads".to_string(),
            path: parent.join("threads"),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: None,
            stub_frontmatter: None,
        },
    ]
}

fn recent_entry(mount_id: &MountId, parent: &Path, channel_id: &str) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: slack_recent_remote_id(channel_id),
        kind: EntityKind::Page,
        title: "Recent".to_string(),
        path: parent.join("recent.md"),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: None,
        stub_frontmatter: None,
    }
}

fn thread_stub_entries(
    mount_id: &MountId,
    parent: &Path,
    channel_id: &str,
    messages: &[SlackMessage],
) -> Vec<TreeEntry> {
    let mut seen = BTreeSet::new();
    messages
        .iter()
        .filter(|message| message.reply_count.unwrap_or(0) > 0)
        .filter_map(|message| {
            let thread_ts = message.thread_ts.as_deref().unwrap_or(&message.ts);
            if !seen.insert(thread_ts.to_string()) {
                return None;
            }
            Some(entry_with_remote_version(
                thread_entry(mount_id, parent, channel_id, thread_ts),
                slack_thread_stub_version(channel_id, thread_ts, message),
            ))
        })
        .collect()
}

fn thread_entry(mount_id: &MountId, parent: &Path, channel_id: &str, thread_ts: &str) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: slack_thread_remote_id(channel_id, thread_ts),
        kind: EntityKind::Page,
        title: format!("Slack thread {thread_ts}"),
        path: parent.join(thread_file_name(thread_ts)),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: None,
        stub_frontmatter: None,
    }
}

fn slack_thread_stub_version(channel_id: &str, thread_ts: &str, message: &SlackMessage) -> String {
    let latest_ts = message.latest_reply.as_deref().unwrap_or(thread_ts);
    format!("slack:{channel_id}:thread:{thread_ts}:{latest_ts}")
}

fn entry_with_remote_version(mut entry: TreeEntry, version: String) -> TreeEntry {
    entry.remote_edited_at = Some(version);
    entry
}

fn observation_from_entry(entry: TreeEntry, parent: Option<RemoteId>) -> RemoteObservation {
    let mut observation = RemoteObservation::new(
        entry.mount_id,
        entry.remote_id,
        entry.kind,
        entry.title,
        entry.path,
    );
    if let Some(parent) = parent {
        observation = observation.with_parent(parent);
    }
    if let Some(version) = entry.remote_edited_at {
        observation = observation.with_remote_version(RemoteVersion::new(version));
    }
    observation
}

fn channel_directory_name(channel: &SlackConversation) -> String {
    safe_filename(&channel.name, 160)
}
