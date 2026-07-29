use std::collections::BTreeMap;
use std::path::{Component, Path};
use std::sync::Arc;

use locality_connector::{Connector, EnumerateRequest, FetchRequest};
use locality_core::LocalityResult;
use locality_core::canonical::render_canonical_markdown;
use locality_core::model::{MountId, RemoteId};
use locality_slack::{
    SLACK_OAUTH_SCOPES, SlackApi, SlackAuthTestResponse, SlackConfig, SlackConnector,
    SlackConversation, SlackConversationsListResponse, SlackFile, SlackHistoryResponse,
    SlackJoinResponse, SlackMessage, SlackMountSettings, SlackOAuthScopeError,
    SlackResponseMetadata, SlackUser, SlackUserProfile, SlackUsersListResponse,
    slack_capabilities_json, validate_slack_oauth_scopes,
};

const DEFAULT_SETTINGS: &[u8] = include_bytes!("../fixtures/direct-v1/settings-default.json");
const CUSTOM_SETTINGS: &[u8] = include_bytes!("../fixtures/direct-v1/settings-custom.json");
const OAUTH_CAPABILITIES: &[u8] = include_bytes!("../fixtures/direct-v1/oauth-capabilities.json");
const OAUTH_SCOPES: &[u8] = include_bytes!("../fixtures/direct-v1/oauth-scopes.json");
const TREE_PATHS: &[u8] = include_bytes!("../fixtures/direct-v1/tree-paths.txt");
const NATIVE_USERS: &[u8] = include_bytes!("../fixtures/direct-v1/native-users.json");
const NATIVE_RECENT: &[u8] = include_bytes!("../fixtures/direct-v1/native-recent.json");
const USERS_MARKDOWN: &[u8] = include_bytes!("../fixtures/direct-v1/users.md");
const RECENT_MARKDOWN: &[u8] = include_bytes!("../fixtures/direct-v1/recent.md");

#[test]
fn direct_v1_settings_bytes_are_frozen() {
    let default = SlackMountSettings::default()
        .to_json()
        .expect("encode default Slack settings");
    assert_json_bytes("default settings", default.as_bytes(), DEFAULT_SETTINGS);

    let custom = SlackMountSettings::from_json(
        r#"{"slack":{"history_limit":9,"types":["im","mpim"],"auto_join_public_channels":true}}"#,
    )
    .expect("parse representative custom Slack settings")
    .to_json()
    .expect("encode representative custom Slack settings");
    assert_json_bytes("custom settings", custom.as_bytes(), CUSTOM_SETTINGS);
}

#[test]
fn direct_v1_desktop_oauth_bytes_are_frozen() {
    let capabilities = slack_capabilities_json().expect("encode Slack capabilities");
    assert_json_bytes(
        "OAuth capabilities",
        capabilities.as_bytes(),
        OAUTH_CAPABILITIES,
    );

    let scopes = serde_json::to_vec(SLACK_OAUTH_SCOPES).expect("encode Slack OAuth scopes");
    assert_json_bytes("OAuth scopes", &scopes, OAUTH_SCOPES);

    let frozen_scopes = serde_json::from_slice::<Vec<String>>(
        OAUTH_SCOPES
            .strip_suffix(b"\n")
            .expect("OAuth scope fixture ends in one review-friendly newline"),
    )
    .expect("decode independently frozen OAuth scopes");
    validate_slack_oauth_scopes(&frozen_scopes)
        .expect("exact frozen OAuth scope list must be accepted");

    for removed_index in 0..frozen_scopes.len() {
        let removed_scope = frozen_scopes[removed_index].clone();
        let mut missing_one = frozen_scopes.clone();
        missing_one.remove(removed_index);
        let error = validate_slack_oauth_scopes(&missing_one)
            .expect_err("removing a required frozen OAuth scope must fail");
        let SlackOAuthScopeError::MissingRequiredScope(missing_scope) = error else {
            panic!("removing {removed_scope} returned the wrong error: {error:?}");
        };
        assert_eq!(missing_scope, removed_scope);
    }

    let mut with_unlisted_scope = frozen_scopes;
    with_unlisted_scope.push("synthetic:unlisted".to_string());
    assert_eq!(
        validate_slack_oauth_scopes(&with_unlisted_scope),
        Err(SlackOAuthScopeError::UnsupportedScope(
            "synthetic:unlisted".to_string()
        ))
    );
}

#[test]
fn direct_v1_tree_native_and_markdown_bytes_are_frozen() {
    let connector = connector();
    let entries = connector
        .enumerate(EnumerateRequest {
            mount_id: MountId::new("slack-main"),
            cursor: None,
        })
        .expect("enumerate complete Slack tree");
    let tree_paths = entries
        .iter()
        .map(|entry| slash_separated_logical_path(&entry.path))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_bytes("logical tree paths", tree_paths.as_bytes(), TREE_PATHS);

    let users = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new("slack-users"),
        })
        .expect("fetch users native entity");
    assert_json_bytes("users native bundle", &users.raw, NATIVE_USERS);
    let users_document = connector.render(&users).expect("render users.md");
    let users_markdown = render_canonical_markdown(&users_document);
    assert_bytes("users.md", users_markdown.as_bytes(), USERS_MARKDOWN);

    let recent = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new("slack-recent:C123"),
        })
        .expect("fetch recent native entity");
    assert_json_bytes("recent native bundle", &recent.raw, NATIVE_RECENT);
    assert!(
        recent
            .raw
            .windows(
                b"\"url_private\":\"https://files.slack.com/files-pri/T123-F123/plan.pdf\"".len()
            )
            .any(|window| window
                == b"\"url_private\":\"https://files.slack.com/files-pri/T123-F123/plan.pdf\""),
        "direct native Slack file data must retain url_private"
    );
    let recent_document = connector.render(&recent).expect("render recent.md");
    let recent_markdown = render_canonical_markdown(&recent_document);
    assert_bytes_with_final_blank_line("recent.md", recent_markdown.as_bytes(), RECENT_MARKDOWN);
}

fn assert_bytes(label: &str, actual: &[u8], expected: &[u8]) {
    assert_eq!(
        actual,
        expected,
        "{label} changed\nactual UTF-8:\n{}",
        String::from_utf8_lossy(actual)
    );
}

fn assert_json_bytes(label: &str, actual: &[u8], fixture: &[u8]) {
    let expected = fixture
        .strip_suffix(b"\n")
        .expect("JSON compatibility fixtures must end in one review-friendly newline");
    assert_bytes(label, actual, expected);
}

fn assert_bytes_with_final_blank_line(label: &str, actual: &[u8], fixture: &[u8]) {
    let mut expected = fixture.to_vec();
    expected.push(b'\n');
    assert_bytes(label, actual, &expected);
}

fn slash_separated_logical_path(path: &Path) -> String {
    path.components()
        .map(|component| match component {
            Component::Normal(segment) => segment
                .to_str()
                .expect("logical Slack path components must be UTF-8"),
            Component::Prefix(_) => panic!("logical Slack paths must not contain a prefix"),
            Component::RootDir => panic!("logical Slack paths must not contain a root"),
            Component::ParentDir => {
                panic!("logical Slack paths must not contain parent traversal")
            }
            Component::CurDir => {
                panic!("logical Slack paths must not contain current-directory components")
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn connector() -> SlackConnector {
    SlackConnector::with_api(
        SlackConfig::new("xoxb-direct-v1-test"),
        Arc::new(FakeSlackApi::new()),
    )
}

#[derive(Debug)]
struct FakeSlackApi {
    conversations: Vec<SlackConversation>,
    users: Vec<SlackUser>,
    messages: BTreeMap<String, Vec<SlackMessage>>,
}

impl FakeSlackApi {
    fn new() -> Self {
        let users = vec![
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
        ];
        let conversations = vec![
            SlackConversation {
                id: "MP123".to_string(),
                name: Some("product-trio".to_string()),
                is_mpim: true,
                updated: Some(1_780_000_003),
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
                id: "C123".to_string(),
                name: Some("general".to_string()),
                is_channel: true,
                updated: Some(1_780_000_000),
                num_members: Some(42),
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
        ];
        let messages = BTreeMap::from([(
            "C123".to_string(),
            vec![SlackMessage {
                r#type: Some("message".to_string()),
                user: Some("U123".to_string()),
                text: "Hello from Slack, <@U456>; see <https://example.com|planning>".to_string(),
                ts: "1780000000.000100".to_string(),
                files: vec![SlackFile {
                    id: "F123".to_string(),
                    name: Some("plan.pdf".to_string()),
                    title: Some("Planning brief".to_string()),
                    mimetype: Some("application/pdf".to_string()),
                    url_private: Some(
                        "https://files.slack.com/files-pri/T123-F123/plan.pdf".to_string(),
                    ),
                    file_access: Some("visible".to_string()),
                }],
                ..SlackMessage::default()
            }],
        )]);
        Self {
            conversations,
            users,
            messages,
        }
    }
}

impl SlackApi for FakeSlackApi {
    fn auth_test(&self) -> LocalityResult<SlackAuthTestResponse> {
        Ok(SlackAuthTestResponse {
            ok: true,
            team: Some("Locality".to_string()),
            team_id: Some("T123".to_string()),
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
            messages: self.messages.get(channel).cloned().unwrap_or_default(),
            ..SlackHistoryResponse::default()
        })
    }

    fn conversations_replies(
        &self,
        _channel: &str,
        _thread_ts: &str,
        _cursor: Option<&str>,
        _limit: u32,
    ) -> LocalityResult<SlackHistoryResponse> {
        Ok(SlackHistoryResponse {
            ok: true,
            ..SlackHistoryResponse::default()
        })
    }

    fn conversations_join(&self, channel: &str) -> LocalityResult<SlackJoinResponse> {
        Ok(SlackJoinResponse {
            ok: true,
            channel: self
                .conversations
                .iter()
                .find(|conversation| conversation.id == channel)
                .cloned(),
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
            members: self.users.clone(),
            ..SlackUsersListResponse::default()
        })
    }
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
