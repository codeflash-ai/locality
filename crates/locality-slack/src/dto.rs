use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackAuthTest {
    pub ok: bool,
    pub url: Option<String>,
    pub team: Option<String>,
    pub user: Option<String>,
    pub team_id: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackConversationList {
    #[serde(default)]
    pub channels: Vec<SlackConversation>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackConversation {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub is_channel: bool,
    #[serde(default)]
    pub is_archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackHistory {
    #[serde(default)]
    pub messages: Vec<SlackMessage>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackMessage {
    pub ts: String,
    pub user: Option<String>,
    #[serde(default)]
    pub text: String,
    pub permalink: Option<String>,
    pub reply_count: Option<u32>,
    pub thread_ts: Option<String>,
    pub latest_reply: Option<String>,
    #[serde(default)]
    pub blocks: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackUserInfo {
    pub user: SlackUser,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackUser {
    pub id: String,
    pub name: String,
    pub real_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SlackConversationListResponse {
    pub ok: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub channels: Vec<SlackConversation>,
    pub response_metadata: Option<SlackResponseMetadata>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SlackConversationInfoResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub channel: Option<SlackConversation>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SlackHistoryResponse {
    pub ok: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub messages: Vec<SlackMessage>,
    pub response_metadata: Option<SlackResponseMetadata>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SlackUserInfoResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub user: Option<SlackUser>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SlackResponseMetadata {
    pub next_cursor: Option<String>,
}

impl From<SlackConversationListResponse> for SlackConversationList {
    fn from(value: SlackConversationListResponse) -> Self {
        Self {
            channels: value.channels,
            next_cursor: value.response_metadata.and_then(|metadata| {
                metadata
                    .next_cursor
                    .filter(|cursor| !cursor.trim().is_empty())
            }),
        }
    }
}

impl From<SlackHistoryResponse> for SlackHistory {
    fn from(value: SlackHistoryResponse) -> Self {
        Self {
            messages: value.messages,
            next_cursor: value.response_metadata.and_then(|metadata| {
                metadata
                    .next_cursor
                    .filter(|cursor| !cursor.trim().is_empty())
            }),
        }
    }
}
