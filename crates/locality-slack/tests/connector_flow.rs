use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use locality_connector::{
    ApplyPlanRequest, ApplyUndoRequest, ChildContainer, Connector, ConnectorCapabilities,
    FetchRequest, ListChildrenRequest,
};
use locality_core::journal::{PushId, PushOperationId};
use locality_core::model::{CanonicalDocument, EntityKind, MountId, RemoteId};
use locality_core::planner::{PushOperation, PushPlan};
use locality_core::undo::{UndoPlan, UndoPlanStatus};
use locality_core::{LocalityError, LocalityResult};
use locality_slack::{
    SLACK_CONNECTOR_ID, SlackApi, SlackAuthTestResponse, SlackConfig, SlackConnector,
    SlackConversation, SlackConversationsListResponse, SlackHistoryResponse, SlackJoinResponse,
    SlackMessage, SlackResponseMetadata, SlackUser, SlackUserProfile, SlackUsersListResponse,
};

#[test]
fn slack_connector_projects_read_only_tree_and_hydrates_recent_markdown() {
    let connector = SlackConnector::with_api(
        SlackConfig::new("xoxb-test"),
        Arc::new(FakeSlackApi::with_fixture()),
    );
    let mount_id = MountId::new("slack-main");

    let root = connector
        .list_children(ListChildrenRequest {
            mount_id: mount_id.clone(),
            container: ChildContainer::Root,
            parent_path: PathBuf::new(),
        })
        .expect("list Slack root");
    assert!(root.is_complete());
    assert_eq!(
        paths(&root.entries),
        vec![
            PathBuf::from("channels"),
            PathBuf::from("private-channels"),
            PathBuf::from("dms"),
            PathBuf::from("group-dms"),
            PathBuf::from("users.md"),
        ]
    );

    let users_entry = root
        .entries
        .iter()
        .find(|entry| entry.path == Path::new("users.md"))
        .expect("users.md root entry");
    assert_eq!(users_entry.kind, EntityKind::Page);
    assert_eq!(users_entry.remote_id.as_str(), "slack-users");
    let users = connector
        .fetch(FetchRequest {
            remote_id: users_entry.remote_id.clone(),
        })
        .expect("fetch users.md");
    let users_document = connector.render(&users).expect("render users.md");
    assert!(users_document.frontmatter.contains("  connector: slack\n"));
    assert!(
        users_document
            .body
            .contains("| U123 | ada | Ada Lovelace | false | false |")
    );
    assert!(
        users_document
            .body
            .contains("| U456 | grace | Grace Hopper | false | false |")
    );

    let expected_recent_paths = BTreeMap::from([
        (
            "slack-recent:C123",
            (
                PathBuf::from("channels/general-C123/recent.md"),
                "Hello from Slack, @Grace Hopper; see [planning](https://example.com)",
            ),
        ),
        (
            "slack-recent:G123",
            (
                PathBuf::from("private-channels/leadership-G123/recent.md"),
                "Private channel planning notes",
            ),
        ),
        (
            "slack-recent:D123",
            (
                PathBuf::from("dms/Ada Lovelace-D123/recent.md"),
                "DM follow up for Ada",
            ),
        ),
        (
            "slack-recent:MP123",
            (
                PathBuf::from("group-dms/product-trio-MP123/recent.md"),
                "Group DM triage update",
            ),
        ),
    ]);

    let mut projected_recent_paths = Vec::new();
    for (folder_remote_id, folder_path) in [
        ("slack-folder:channels", "channels"),
        ("slack-folder:private-channels", "private-channels"),
        ("slack-folder:dms", "dms"),
        ("slack-folder:group-dms", "group-dms"),
    ] {
        let conversations = connector
            .list_children(ListChildrenRequest {
                mount_id: mount_id.clone(),
                container: ChildContainer::DirectoryChildren(RemoteId::new(folder_remote_id)),
                parent_path: PathBuf::from(folder_path),
            })
            .expect("list Slack conversation bucket");
        assert!(conversations.is_complete());
        assert_eq!(conversations.entries.len(), 1);
        assert_eq!(conversations.entries[0].kind, EntityKind::Directory);

        let recent = connector
            .list_children(ListChildrenRequest {
                mount_id: mount_id.clone(),
                container: ChildContainer::DirectoryChildren(
                    conversations.entries[0].remote_id.clone(),
                ),
                parent_path: conversations.entries[0].path.clone(),
            })
            .expect("list Slack conversation directory");
        assert!(recent.is_complete());
        assert_eq!(recent.entries.len(), 1);
        assert_eq!(recent.entries[0].kind, EntityKind::Page);
        assert_eq!(recent.entries[0].title, "recent");

        let expected = expected_recent_paths
            .get(recent.entries[0].remote_id.as_str())
            .expect("expected recent path for fixture conversation");
        assert_eq!(&recent.entries[0].path, &expected.0);
        projected_recent_paths.push(recent.entries[0].path.clone());

        let native = connector
            .fetch(FetchRequest {
                remote_id: recent.entries[0].remote_id.clone(),
            })
            .expect("fetch recent.md");
        let document = connector.render(&native).expect("render recent.md");
        assert!(document.frontmatter.contains("  connector: slack\n"));
        assert!(document.frontmatter.contains(&format!(
            "  conversation_id: \"{}\"",
            conversation_id_yaml(&recent.entries[0].remote_id)
        )));
        assert!(
            document.body.contains(expected.1),
            "body was:\n{}",
            document.body
        );
    }

    projected_recent_paths.sort();
    let mut expected_projected_recent_paths = expected_recent_paths
        .values()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    expected_projected_recent_paths.sort();
    assert_eq!(projected_recent_paths, expected_projected_recent_paths);
}

#[test]
fn slack_connector_rejects_write_paths_exposed_by_the_crate() {
    let connector = SlackConnector::with_api(
        SlackConfig::new("xoxb-test"),
        Arc::new(FakeSlackApi::with_fixture()),
    );

    assert_eq!(connector.kind().0, SLACK_CONNECTOR_ID);
    assert_eq!(
        connector.capabilities(),
        ConnectorCapabilities {
            supports_oauth: true,
            ..ConnectorCapabilities::read_only()
        }
    );
    assert!(connector.supported_push_operations().is_empty());
    assert_unsupported_writes(
        connector.parse(&CanonicalDocument::new(
            "loc:\n  id: slack-recent:C123\n  type: page\n  connector: slack\n".to_string(),
            "edited body".to_string(),
        )),
        "parse",
    );

    let push_id = PushId("push-slack-readonly-test".to_string());
    let mount_id = MountId::new("slack-main");
    let operation = PushOperation::UpdateProperties {
        entity_id: RemoteId::new("slack-recent:C123"),
        properties: BTreeMap::new(),
    };
    let operation_id = PushOperationId::for_operation(&push_id, 0, &operation);
    let plan = PushPlan::new(vec![RemoteId::new("slack-recent:C123")], vec![operation]);

    assert_unsupported_writes(
        connector.check_concurrency(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: std::slice::from_ref(&operation_id),
            remote_preconditions: &[],
            local_root: None,
        }),
        "check_concurrency",
    );
    assert_unsupported_writes(
        connector.apply(ApplyPlanRequest {
            push_id: &push_id,
            mount_id: &mount_id,
            plan: &plan,
            operation_ids: &[operation_id],
            remote_preconditions: &[],
            local_root: None,
        }),
        "apply",
    );

    let undo_plan = UndoPlan {
        target_push_id: push_id.clone(),
        mount_id: mount_id.clone(),
        affected_entities: vec![RemoteId::new("slack-recent:C123")],
        operations: Vec::new(),
        unsupported: Vec::new(),
        status: UndoPlanStatus::Complete,
    };
    assert_unsupported_writes(
        connector.apply_undo(ApplyUndoRequest {
            target_push_id: &push_id,
            mount_id: &mount_id,
            plan: &undo_plan,
        }),
        "apply_undo",
    );
}

#[derive(Clone, Debug)]
struct FakeSlackApi {
    conversations: Arc<Vec<SlackConversation>>,
    users: Arc<Vec<SlackUser>>,
    messages: Arc<Mutex<BTreeMap<String, Vec<SlackMessage>>>>,
}

impl FakeSlackApi {
    fn with_fixture() -> Self {
        let users = vec![
            SlackUser {
                id: "U123".to_string(),
                name: Some("ada".to_string()),
                real_name: Some("Ada Lovelace".to_string()),
                profile: Some(SlackUserProfile {
                    real_name: Some("Ada Lovelace".to_string()),
                    display_name: Some("ada".to_string()),
                    email: None,
                }),
                ..SlackUser::default()
            },
            SlackUser {
                id: "U456".to_string(),
                name: Some("grace".to_string()),
                real_name: Some("Grace Hopper".to_string()),
                profile: Some(SlackUserProfile {
                    real_name: Some("Grace Hopper".to_string()),
                    display_name: Some("grace".to_string()),
                    email: None,
                }),
                ..SlackUser::default()
            },
        ];
        let conversations = vec![
            SlackConversation {
                id: "C123".to_string(),
                name: Some("general".to_string()),
                is_channel: true,
                updated: Some(1_780_000_000),
                ..SlackConversation::default()
            },
            SlackConversation {
                id: "G123".to_string(),
                name: Some("leadership".to_string()),
                is_group: true,
                is_private: true,
                updated: Some(1_780_000_001),
                ..SlackConversation::default()
            },
            SlackConversation {
                id: "D123".to_string(),
                user: Some("U123".to_string()),
                is_im: true,
                updated: Some(1_780_000_002),
                ..SlackConversation::default()
            },
            SlackConversation {
                id: "MP123".to_string(),
                name: Some("product-trio".to_string()),
                is_mpim: true,
                updated: Some(1_780_000_003),
                ..SlackConversation::default()
            },
        ];
        let messages = BTreeMap::from([
            (
                "C123".to_string(),
                vec![SlackMessage {
                    user: Some("U123".to_string()),
                    text: "Hello from Slack, <@U456>; see <https://example.com|planning>"
                        .to_string(),
                    ts: "1780000000.000100".to_string(),
                    ..SlackMessage::default()
                }],
            ),
            (
                "G123".to_string(),
                vec![SlackMessage {
                    user: Some("U456".to_string()),
                    text: "Private channel planning notes".to_string(),
                    ts: "1780000001.000100".to_string(),
                    ..SlackMessage::default()
                }],
            ),
            (
                "D123".to_string(),
                vec![SlackMessage {
                    user: Some("U123".to_string()),
                    text: "DM follow up for Ada".to_string(),
                    ts: "1780000002.000100".to_string(),
                    ..SlackMessage::default()
                }],
            ),
            (
                "MP123".to_string(),
                vec![SlackMessage {
                    user: Some("U456".to_string()),
                    text: "Group DM triage update".to_string(),
                    ts: "1780000003.000100".to_string(),
                    ..SlackMessage::default()
                }],
            ),
        ]);
        Self {
            conversations: Arc::new(conversations),
            users: Arc::new(users),
            messages: Arc::new(Mutex::new(messages)),
        }
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
        Ok(SlackConversationsListResponse {
            ok: true,
            channels: self
                .conversations
                .iter()
                .filter(|conversation| conversation_matches_types(conversation, types))
                .cloned()
                .collect(),
            response_metadata: SlackResponseMetadata::default(),
            error: None,
        })
    }

    fn conversations_history(
        &self,
        channel: &str,
        _cursor: Option<&str>,
        _limit: u32,
    ) -> LocalityResult<SlackHistoryResponse> {
        Ok(SlackHistoryResponse {
            ok: true,
            messages: self
                .messages
                .lock()
                .expect("messages")
                .get(channel)
                .cloned()
                .unwrap_or_default(),
            ..SlackHistoryResponse::default()
        })
    }

    fn conversations_replies(
        &self,
        channel: &str,
        thread_ts: &str,
        _cursor: Option<&str>,
        _limit: u32,
    ) -> LocalityResult<SlackHistoryResponse> {
        Ok(SlackHistoryResponse {
            ok: true,
            messages: self
                .messages
                .lock()
                .expect("messages")
                .get(channel)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|message| {
                    message.ts == thread_ts || message.thread_ts.as_deref() == Some(thread_ts)
                })
                .collect(),
            ..SlackHistoryResponse::default()
        })
    }

    fn conversations_join(&self, channel: &str) -> LocalityResult<SlackJoinResponse> {
        let channel = self
            .conversations
            .iter()
            .find(|conversation| conversation.id == channel)
            .cloned();
        Ok(SlackJoinResponse {
            ok: true,
            channel,
            ..SlackJoinResponse::default()
        })
    }

    fn users_list(
        &self,
        _cursor: Option<&str>,
        _limit: u32,
    ) -> LocalityResult<SlackUsersListResponse> {
        Ok(SlackUsersListResponse {
            ok: true,
            members: self.users.as_ref().clone(),
            ..SlackUsersListResponse::default()
        })
    }
}

fn paths(entries: &[locality_core::model::TreeEntry]) -> Vec<PathBuf> {
    entries.iter().map(|entry| entry.path.clone()).collect()
}

fn conversation_matches_types(conversation: &SlackConversation, types: &str) -> bool {
    types.split(',').any(|conversation_type| {
        matches!(
            conversation_type,
            "public_channel" if conversation.is_channel && !conversation.is_private
        ) || matches!(
            conversation_type,
            "private_channel" if (conversation.is_group || conversation.is_private)
                && !conversation.is_mpim
        ) || matches!(conversation_type, "im" if conversation.is_im)
            || matches!(conversation_type, "mpim" if conversation.is_mpim)
    })
}

fn conversation_id_yaml(remote_id: &RemoteId) -> &str {
    remote_id
        .as_str()
        .strip_prefix("slack-recent:")
        .expect("recent remote id")
}

fn assert_unsupported_writes<T>(result: LocalityResult<T>, operation: &str) {
    assert!(
        matches!(result, Err(LocalityError::Unsupported("Slack writes"))),
        "{operation} should reject Slack writes"
    );
}
