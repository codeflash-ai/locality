use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackResponseMetadata {
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackTopic {
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackConversation {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub is_channel: bool,
    #[serde(default)]
    pub is_group: bool,
    #[serde(default)]
    pub is_im: bool,
    #[serde(default)]
    pub is_mpim: bool,
    #[serde(default)]
    pub is_private: bool,
    #[serde(default)]
    pub is_member: Option<bool>,
    #[serde(default)]
    pub is_archived: bool,
    #[serde(default)]
    pub updated: Option<u64>,
    #[serde(default)]
    pub num_members: Option<u64>,
    #[serde(default)]
    pub topic: Option<SlackTopic>,
    #[serde(default)]
    pub purpose: Option<SlackTopic>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackConversationsListResponse {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub channels: Vec<SlackConversation>,
    #[serde(default)]
    pub response_metadata: SlackResponseMetadata,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackFile {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub mimetype: Option<String>,
    #[serde(default)]
    pub url_private: Option<String>,
    #[serde(default)]
    pub file_access: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SlackMessage {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub text: String,
    pub ts: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub reply_count: Option<u64>,
    #[serde(default)]
    pub files: Vec<SlackFile>,
    #[serde(default)]
    pub blocks: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SlackHistoryResponse {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub messages: Vec<SlackMessage>,
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub response_metadata: SlackResponseMetadata,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackJoinResponse {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub channel: Option<SlackConversation>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackUserProfile {
    #[serde(default)]
    pub real_name: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackUser {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub real_name: Option<String>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub profile: Option<SlackUserProfile>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackUsersListResponse {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub members: Vec<SlackUser>,
    #[serde(default)]
    pub response_metadata: SlackResponseMetadata,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackAuthTestResponse {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub team: Option<String>,
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_conversations_list_response() {
        let raw = r#"{
          "ok": true,
          "channels": [
            {
              "id": "C123",
              "name": "general",
              "is_channel": true,
              "is_group": false,
              "is_im": false,
              "is_mpim": false,
              "is_archived": false,
              "is_private": false,
              "updated": 1780000000000000,
              "num_members": 12,
              "topic": { "value": "Company-wide updates" },
              "purpose": { "value": "Announcements" }
            }
          ],
          "response_metadata": { "next_cursor": "abc" }
        }"#;

        let page: SlackConversationsListResponse = serde_json::from_str(raw).expect("decode");
        assert!(page.ok);
        assert_eq!(page.channels[0].id, "C123");
        assert_eq!(page.channels[0].name.as_deref(), Some("general"));
        assert_eq!(page.response_metadata.next_cursor.as_deref(), Some("abc"));
    }

    #[test]
    fn decodes_history_response_with_file_share() {
        let raw = r#"{
          "ok": true,
          "messages": [
            {
              "type": "message",
              "user": "U123",
              "text": "hello <https://example.com|example>",
              "ts": "1780000000.000100",
              "thread_ts": "1780000000.000100",
              "reply_count": 2,
              "files": [
                {
                  "id": "F123",
                  "name": "plan.pdf",
                  "title": "Plan",
                  "mimetype": "application/pdf",
                  "url_private": "https://files.slack.com/files-pri/T/F"
                }
              ]
            }
          ],
          "has_more": false,
          "response_metadata": { "next_cursor": "" }
        }"#;

        let history: SlackHistoryResponse = serde_json::from_str(raw).expect("decode");
        assert_eq!(
            history.messages[0].files[0].name.as_deref(),
            Some("plan.pdf")
        );
        assert_eq!(history.messages[0].reply_count, Some(2));
    }
}
