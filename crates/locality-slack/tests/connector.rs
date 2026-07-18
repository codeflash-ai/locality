use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use locality_connector::{
    ApplyPlanRequest, ApplyUndoRequest, ChildContainer, Connector, EnumerateRequest, FetchRequest,
    ListChildrenRequest, ObserveRequest,
};
use locality_core::LocalityError;
use locality_core::journal::PushId;
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::PushPlan;
use locality_core::undo::{UndoPlan, UndoPlanStatus};
use locality_slack::client::SlackApi;
use locality_slack::connector::{SlackConfig, SlackConnector};
use locality_slack::dto::{
    SlackAuthTest, SlackConversation, SlackConversationList, SlackHistory, SlackMessage, SlackUser,
    SlackUserInfo,
};
use locality_slack::render::{
    SlackContentKind, SlackNativeBundle, render_slack_document, slack_recent_remote_id,
    slack_thread_remote_id,
};
use locality_slack::settings::SlackMountSettings;

#[test]
fn default_settings_are_public_channel_recent_limit_15() {
    let settings = SlackMountSettings::default();

    assert_eq!(settings.recent_limit, 15);
    assert_eq!(settings.conversation_types, "public_channel");
    assert_eq!(SlackMountSettings::from_json("{}").unwrap(), settings);
}

#[test]
fn connector_lists_channel_tree_and_thread_stubs() {
    let api = Arc::new(FakeSlackApi::default());
    let connector = SlackConnector::with_api(SlackConfig::new("token"), api.clone());
    let mount_id = MountId::new("slack-main");

    let entries = connector
        .enumerate(EnumerateRequest {
            mount_id: mount_id.clone(),
            cursor: None,
        })
        .expect("enumerate channels");

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path, std::path::Path::new("channels"));
    assert_eq!(entries[0].kind, EntityKind::Directory);
    assert_eq!(entries[0].hydration, HydrationState::Stub);
    assert_eq!(entries[1].path, std::path::Path::new("channels/general"));
    assert_eq!(entries[1].remote_id, RemoteId::new("slack-channel:C123"));

    let channel_children = connector
        .list_children(ListChildrenRequest {
            mount_id: mount_id.clone(),
            container: ChildContainer::DirectoryChildren(RemoteId::new("slack-channel:C123")),
            parent_path: "channels/general".into(),
        })
        .expect("channel children");
    let paths = channel_children
        .entries
        .iter()
        .map(|entry| entry.path.display().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec!["channels/general/recent.md", "channels/general/threads"]
    );

    let thread_children = connector
        .list_children(ListChildrenRequest {
            mount_id,
            container: ChildContainer::DirectoryChildren(RemoteId::new("slack-threads:C123")),
            parent_path: "channels/general/threads".into(),
        })
        .expect("thread children");
    assert_eq!(thread_children.entries.len(), 1);
    assert_eq!(
        thread_children.entries[0].path,
        std::path::Path::new("channels/general/threads/2026-07-17-15.22.10-1784301730.000100.md")
    );
    assert_eq!(
        thread_children.entries[0].remote_id,
        slack_thread_remote_id("C123", "1784301730.000100")
    );
    assert_eq!(
        thread_children.entries[0].remote_edited_at.as_deref(),
        Some("slack:C123:thread:1784301730.000100:1794842540.000200")
    );

    let calls = api.calls.lock().expect("calls");
    assert_eq!(calls.list_channels, vec![(15, None)]);
    assert_eq!(calls.history, vec![("C123".to_string(), 15, None)]);
}

#[test]
fn fetch_recent_and_thread_render_as_canonical_markdown() {
    let api = Arc::new(FakeSlackApi::default());
    let connector = SlackConnector::with_api(SlackConfig::new("token"), api);

    let recent = connector
        .fetch(FetchRequest {
            remote_id: slack_recent_remote_id("C123"),
        })
        .and_then(|native| connector.render(&native))
        .expect("render recent");

    assert!(recent.frontmatter.contains("connector: slack"));
    assert!(recent.frontmatter.contains("content_kind: recent"));
    assert!(recent.frontmatter.contains("channel_id: \"C123\""));
    assert!(
        recent
            .frontmatter
            .contains("latest_ts: \"1794842540.000200\"")
    );
    assert!(
        recent
            .frontmatter
            .contains("remote_edited_at: \"slack:C123:recent:1794842540.000200\"")
    );
    assert!(
        recent
            .body
            .contains("## 2026-07-17 15:22:10 UTC - Ada Lovelace")
    );
    assert!(recent.body.contains("Hello from Slack"));
    assert!(recent.body.contains("2 replies"));
    assert!(
        recent
            .body
            .contains("https://example.slack.com/archives/C123/p1784301730000100")
    );
    assert!(recent.body.contains("`C123/1784301730.000100`"));

    let thread = connector
        .fetch(FetchRequest {
            remote_id: slack_thread_remote_id("C123", "1784301730.000100"),
        })
        .and_then(|native| connector.render(&native))
        .expect("render thread");

    assert!(thread.frontmatter.contains("content_kind: thread"));
    assert!(
        thread
            .frontmatter
            .contains("thread_ts: \"1784301730.000100\"")
    );
    assert!(
        thread.frontmatter.contains(
            "remote_edited_at: \"slack:C123:thread:1784301730.000100:1794842540.000200\""
        )
    );
    assert!(
        thread.body.find("Hello from Slack").unwrap() < thread.body.find("Thread reply").unwrap()
    );
    assert!(
        thread
            .body
            .contains("::loc{{unsupported source=\"slack\" kind=\"rich_block\"}}")
    );
}

#[test]
fn fetch_known_channel_documents_do_not_enumerate_every_channel() {
    let api = Arc::new(FakeSlackApi::default());
    let connector = SlackConnector::with_api(SlackConfig::new("token"), api.clone());

    connector
        .fetch(FetchRequest {
            remote_id: slack_recent_remote_id("C123"),
        })
        .expect("fetch recent");
    connector
        .fetch(FetchRequest {
            remote_id: slack_thread_remote_id("C123", "1784301730.000100"),
        })
        .expect("fetch thread");

    let calls = api.calls.lock().expect("calls");
    assert_eq!(calls.list_channels, Vec::<(u32, Option<String>)>::new());
    assert_eq!(
        calls.channel_info,
        vec!["C123".to_string(), "C123".to_string()]
    );
    assert_eq!(calls.history.len(), 1);
    assert_eq!(calls.replies.len(), 1);
}

#[test]
fn observe_recent_and_thread_reports_current_message_versions() {
    let api = Arc::new(FakeSlackApi::default());
    let connector = SlackConnector::with_api(SlackConfig::new("token"), api.clone());
    let mount_id = MountId::new("slack-main");

    let recent = connector
        .observe(ObserveRequest {
            mount_id: mount_id.clone(),
            remote_id: slack_recent_remote_id("C123"),
        })
        .expect("observe recent");
    assert_eq!(
        recent
            .remote_version
            .as_ref()
            .map(|version| version.as_str()),
        Some("slack:C123:recent:1794842540.000200")
    );

    let thread = connector
        .observe(ObserveRequest {
            mount_id,
            remote_id: slack_thread_remote_id("C123", "1784301730.000100"),
        })
        .expect("observe thread");
    assert_eq!(
        thread
            .remote_version
            .as_ref()
            .map(|version| version.as_str()),
        Some("slack:C123:thread:1784301730.000100:1794842540.000200")
    );

    let calls = api.calls.lock().expect("calls");
    assert_eq!(
        calls.channel_info,
        vec!["C123".to_string(), "C123".to_string()]
    );
    assert_eq!(calls.history.len(), 1);
    assert_eq!(calls.replies.len(), 1);
}

#[test]
fn render_preserves_slack_ids_and_readable_block_fallbacks() {
    let bundle = SlackNativeBundle {
        content_kind: SlackContentKind::Thread,
        channel_id: "C123".to_string(),
        channel_name: "general".to_string(),
        recent_limit: 15,
        thread_ts: Some("1784301730.000100".to_string()),
        messages: vec![SlackMessage {
            ts: "1784301730.000100".to_string(),
            user: Some("U123".to_string()),
            text: "".to_string(),
            permalink: Some(
                "https://example.slack.com/archives/C123/p1784301730000100".to_string(),
            ),
            reply_count: Some(1),
            thread_ts: Some("1784301730.000100".to_string()),
            latest_reply: None,
            blocks: vec![serde_json::json!({
                "type": "section",
                "text": { "type": "mrkdwn", "text": "*Block* text" }
            })],
        }],
        users: [(
            "U123".to_string(),
            SlackUser {
                id: "U123".to_string(),
                name: "ada".to_string(),
                real_name: Some("Ada Lovelace".to_string()),
            },
        )]
        .into_iter()
        .collect(),
    };

    let document = render_slack_document(&bundle).expect("render");

    assert!(document.frontmatter.contains("loc:"));
    assert!(document.frontmatter.contains("connector: slack"));
    assert!(document.frontmatter.contains("slack:"));
    assert!(
        document
            .frontmatter
            .contains("thread_ts: \"1784301730.000100\"")
    );
    assert!(document.body.contains("Ada Lovelace"));
    assert!(document.body.contains("*Block* text"));
    assert!(document.body.contains("Slack ID: `C123/1784301730.000100`"));
}

#[test]
fn read_only_connector_rejects_all_push_methods() {
    let connector =
        SlackConnector::with_api(SlackConfig::new("token"), Arc::new(FakeSlackApi::default()));

    assert_eq!(connector.supported_push_operations(), BTreeSet::new());
    assert!(matches!(
        connector.parse(&locality_core::model::CanonicalDocument::new(String::new(), String::new())),
        Err(LocalityError::Unsupported(message)) if message == "Slack connector is read-only in v1"
    ));

    let push_id = PushId("push-1".to_string());
    let mount_id = MountId::new("slack-main");
    let plan = PushPlan::default();
    let apply_request = ApplyPlanRequest {
        push_id: &push_id,
        mount_id: &mount_id,
        plan: &plan,
        operation_ids: &[],
        remote_preconditions: &[],
        local_root: None,
    };
    assert!(matches!(
        connector.check_concurrency(apply_request.clone()),
        Err(LocalityError::Unsupported(message)) if message == "Slack connector is read-only in v1"
    ));
    assert!(matches!(
        connector.apply(apply_request),
        Err(LocalityError::Unsupported(message)) if message == "Slack connector is read-only in v1"
    ));

    let undo_plan = UndoPlan {
        target_push_id: push_id.clone(),
        mount_id: mount_id.clone(),
        affected_entities: Vec::new(),
        operations: Vec::new(),
        unsupported: Vec::new(),
        status: UndoPlanStatus::Complete,
    };
    let undo_request = ApplyUndoRequest {
        target_push_id: &push_id,
        mount_id: &mount_id,
        plan: &undo_plan,
    };
    assert!(matches!(
        connector.apply_undo(undo_request),
        Err(LocalityError::Unsupported(message)) if message == "Slack connector is read-only in v1"
    ));
}

#[derive(Debug, Default)]
struct FakeSlackApi {
    calls: Mutex<FakeCalls>,
}

#[derive(Debug, Default)]
struct FakeCalls {
    channel_info: Vec<String>,
    list_channels: Vec<(u32, Option<String>)>,
    history: Vec<(String, u32, Option<String>)>,
    replies: Vec<(String, String, u32, Option<String>)>,
    users: Vec<String>,
}

impl SlackApi for FakeSlackApi {
    fn auth_test(&self) -> locality_core::LocalityResult<SlackAuthTest> {
        Ok(SlackAuthTest {
            ok: true,
            url: Some("https://example.slack.com/".to_string()),
            team: Some("Example".to_string()),
            user: Some("locality".to_string()),
            team_id: Some("T123".to_string()),
            user_id: Some("U999".to_string()),
        })
    }

    fn list_public_channels(
        &self,
        limit: u32,
        cursor: Option<&str>,
    ) -> locality_core::LocalityResult<SlackConversationList> {
        self.calls
            .lock()
            .expect("calls")
            .list_channels
            .push((limit, cursor.map(str::to_string)));
        Ok(SlackConversationList {
            channels: vec![SlackConversation {
                id: "C123".to_string(),
                name: "general".to_string(),
                is_channel: true,
                is_archived: false,
            }],
            next_cursor: None,
        })
    }

    fn conversation_info(&self, channel: &str) -> locality_core::LocalityResult<SlackConversation> {
        self.calls
            .lock()
            .expect("calls")
            .channel_info
            .push(channel.to_string());
        Ok(SlackConversation {
            id: channel.to_string(),
            name: "general".to_string(),
            is_channel: true,
            is_archived: false,
        })
    }

    fn conversation_history(
        &self,
        channel: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> locality_core::LocalityResult<SlackHistory> {
        self.calls.lock().expect("calls").history.push((
            channel.to_string(),
            limit,
            cursor.map(str::to_string),
        ));
        Ok(SlackHistory {
            messages: vec![root_message(), reply_message()],
            next_cursor: None,
        })
    }

    fn conversation_replies(
        &self,
        channel: &str,
        thread_ts: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> locality_core::LocalityResult<SlackHistory> {
        self.calls.lock().expect("calls").replies.push((
            channel.to_string(),
            thread_ts.to_string(),
            limit,
            cursor.map(str::to_string),
        ));
        Ok(SlackHistory {
            messages: vec![root_message(), reply_message()],
            next_cursor: None,
        })
    }

    fn user_info(&self, user: &str) -> locality_core::LocalityResult<SlackUserInfo> {
        self.calls
            .lock()
            .expect("calls")
            .users
            .push(user.to_string());
        Ok(SlackUserInfo {
            user: SlackUser {
                id: user.to_string(),
                name: "ada".to_string(),
                real_name: Some(
                    if user == "U456" {
                        "Grace Hopper"
                    } else {
                        "Ada Lovelace"
                    }
                    .to_string(),
                ),
            },
        })
    }
}

fn root_message() -> SlackMessage {
    SlackMessage {
        ts: "1784301730.000100".to_string(),
        user: Some("U123".to_string()),
        text: "Hello from Slack".to_string(),
        permalink: Some("https://example.slack.com/archives/C123/p1784301730000100".to_string()),
        reply_count: Some(2),
        thread_ts: Some("1784301730.000100".to_string()),
        latest_reply: Some("1794842540.000200".to_string()),
        blocks: Vec::new(),
    }
}

fn reply_message() -> SlackMessage {
    SlackMessage {
        ts: "1794842540.000200".to_string(),
        user: Some("U456".to_string()),
        text: "Thread reply".to_string(),
        permalink: None,
        reply_count: None,
        thread_ts: Some("1784301730.000100".to_string()),
        latest_reply: None,
        blocks: vec![serde_json::json!({"type": "file", "file_id": "F123"})],
    }
}
