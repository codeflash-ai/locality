#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SlackConversationType {
    PublicChannel,
    PrivateChannel,
    DirectMessage,
    GroupDirectMessage,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SlackMountSettings {
    pub history_limit: usize,
    pub conversation_types: Vec<SlackConversationType>,
}

impl Default for SlackMountSettings {
    fn default() -> Self {
        Self {
            history_limit: 15,
            conversation_types: vec![
                SlackConversationType::PublicChannel,
                SlackConversationType::PrivateChannel,
                SlackConversationType::DirectMessage,
                SlackConversationType::GroupDirectMessage,
            ],
        }
    }
}
