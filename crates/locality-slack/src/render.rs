#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlackNativeBundle;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlackRenderedKind {
    Folder,
    Recent,
    Users,
}

pub fn conversation_remote_id(conversation_id: &str) -> String {
    format!("slack-conversation:{conversation_id}")
}

pub fn recent_remote_id(conversation_id: &str) -> String {
    format!("slack-recent:{conversation_id}")
}

pub fn users_remote_id() -> &'static str {
    "slack-users"
}

pub fn render_slack_entity() -> String {
    String::new()
}
