use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GranolaNoteList {
    #[serde(default)]
    pub notes: Vec<GranolaNoteSummary>,
    #[serde(default)]
    pub has_more: bool,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GranolaNoteSummary {
    pub id: String,
    #[serde(default)]
    pub object: String,
    pub title: Option<String>,
    pub owner: GranolaUser,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GranolaNote {
    pub id: String,
    #[serde(default)]
    pub object: String,
    pub title: Option<String>,
    pub owner: GranolaUser,
    pub created_at: String,
    pub updated_at: String,
    pub web_url: String,
    pub calendar_event: Option<GranolaCalendarEvent>,
    #[serde(default)]
    pub attendees: Vec<GranolaUser>,
    #[serde(default)]
    pub folder_membership: Vec<GranolaFolder>,
    #[serde(default)]
    pub summary_text: String,
    pub summary_markdown: Option<String>,
    pub transcript: Option<Vec<GranolaTranscriptChunk>>,
}

impl From<&GranolaNote> for GranolaNoteSummary {
    fn from(note: &GranolaNote) -> Self {
        Self {
            id: note.id.clone(),
            object: note.object.clone(),
            title: note.title.clone(),
            owner: note.owner.clone(),
            created_at: note.created_at.clone(),
            updated_at: note.updated_at.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GranolaUser {
    pub name: Option<String>,
    pub email: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GranolaCalendarEvent {
    pub event_title: Option<String>,
    #[serde(default)]
    pub invitees: Vec<GranolaCalendarInvitee>,
    pub organiser: Option<String>,
    pub calendar_event_id: Option<String>,
    pub scheduled_start_time: Option<String>,
    pub scheduled_end_time: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GranolaCalendarInvitee {
    pub email: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GranolaFolder {
    pub id: String,
    #[serde(default)]
    pub object: String,
    pub name: String,
    pub parent_folder_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GranolaTranscriptChunk {
    pub speaker: GranolaSpeaker,
    pub text: String,
    pub start_time: String,
    pub end_time: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GranolaSpeaker {
    pub source: String,
    pub diarization_label: Option<String>,
    pub name: Option<String>,
}
