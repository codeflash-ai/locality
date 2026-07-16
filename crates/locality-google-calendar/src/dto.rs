use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEventList {
    #[serde(default)]
    pub items: Vec<CalendarEvent>,
    pub next_page_token: Option<String>,
    pub next_sync_token: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEvent {
    pub kind: Option<String>,
    pub etag: Option<String>,
    pub id: Option<String>,
    pub status: Option<String>,
    pub html_link: Option<String>,
    pub created: Option<String>,
    pub updated: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub color_id: Option<String>,
    pub creator: Option<Value>,
    pub organizer: Option<Value>,
    pub start: Option<EventDateTime>,
    pub end: Option<EventDateTime>,
    pub end_time_unspecified: Option<bool>,
    #[serde(default)]
    pub recurrence: Vec<String>,
    pub recurring_event_id: Option<String>,
    pub original_start_time: Option<EventDateTime>,
    pub transparency: Option<String>,
    pub visibility: Option<String>,
    pub i_cal_uid: Option<String>,
    pub sequence: Option<i64>,
    #[serde(default)]
    pub attendees: Vec<EventAttendee>,
    pub attendees_omitted: Option<bool>,
    pub extended_properties: Option<Value>,
    pub hangout_link: Option<String>,
    pub conference_data: Option<Value>,
    pub reminders: Option<Value>,
    pub source: Option<Value>,
    #[serde(default)]
    pub attachments: Vec<Value>,
    pub event_type: Option<String>,
    pub working_location_properties: Option<Value>,
    pub out_of_office_properties: Option<Value>,
    pub focus_time_properties: Option<Value>,
    pub birthday_properties: Option<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventDateTime {
    pub date: Option<String>,
    pub date_time: Option<String>,
    pub time_zone: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventAttendee {
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub response_status: Option<String>,
    pub optional: Option<bool>,
    pub comment: Option<String>,
    pub additional_guests: Option<i64>,
    pub resource: Option<bool>,
    pub organizer: Option<bool>,
    pub self_: Option<bool>,
    pub id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEventCreateRequest {
    pub id: Option<String>,
    pub summary: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: EventDateTime,
    pub end: EventDateTime,
    #[serde(default)]
    pub attendees: Vec<EventAttendee>,
    #[serde(default)]
    pub recurrence: Vec<String>,
    pub reminders: Option<Value>,
    pub transparency: Option<String>,
    pub visibility: Option<String>,
    pub conference_data: Option<Value>,
    pub extended_properties: Option<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl CalendarEvent {
    pub fn remote_version(&self) -> String {
        format!(
            "google-calendar:{}:{}:{}",
            self.id.as_deref().unwrap_or(""),
            self.updated.as_deref().unwrap_or(""),
            self.etag.as_deref().unwrap_or("")
        )
    }

    pub fn title(&self) -> String {
        self.summary
            .as_ref()
            .filter(|summary| !summary.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| "(no title)".to_string())
    }

    pub fn sort_start_key(&self) -> String {
        self.start
            .as_ref()
            .map(EventDateTime::sort_key)
            .unwrap_or_else(|| "0000-00-00T00:00:00Z".to_string())
    }
}

impl EventDateTime {
    pub fn sort_key(&self) -> String {
        self.date_time
            .clone()
            .or_else(|| self.date.as_ref().map(|date| format!("{date}T00:00:00Z")))
            .unwrap_or_else(|| "0000-00-00T00:00:00Z".to_string())
    }

    pub fn is_present(&self) -> bool {
        self.date
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
            || self
                .date_time
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty())
    }
}
