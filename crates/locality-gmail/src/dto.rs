use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessageList {
    #[serde(default)]
    pub messages: Vec<GmailMessageRef>,
    pub next_page_token: Option<String>,
    pub result_size_estimate: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessageRef {
    pub id: String,
    pub thread_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessage {
    pub id: String,
    pub thread_id: Option<String>,
    #[serde(default)]
    pub label_ids: Vec<String>,
    pub snippet: Option<String>,
    pub internal_date: Option<String>,
    pub payload: Option<GmailMessagePart>,
    pub raw: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessagePart {
    pub part_id: Option<String>,
    pub mime_type: Option<String>,
    pub filename: Option<String>,
    #[serde(default)]
    pub headers: Vec<GmailHeader>,
    pub body: Option<GmailMessagePartBody>,
    #[serde(default)]
    pub parts: Vec<GmailMessagePart>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailHeader {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailMessagePartBody {
    pub size: Option<u64>,
    pub data: Option<String>,
    pub attachment_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailDraftCreateRequest {
    pub message: GmailRawMessage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailDraftSendRequest {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailRawMessage {
    pub raw: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GmailDraft {
    pub id: String,
    pub message: GmailMessage,
}

pub fn header_map(part: &GmailMessagePart) -> BTreeMap<String, String> {
    part.headers
        .iter()
        .map(|header| (header.name.to_ascii_lowercase(), header.value.clone()))
        .collect()
}
