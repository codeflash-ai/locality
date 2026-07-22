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
    #[serde(rename = "iCalUID")]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventAttendee {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_guests: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organizer: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub async_operation: Option<String>,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalendarEventCreateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub start: EventDateTime,
    pub end: EventDateTime,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<EventAttendee>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recurrence: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reminders: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transparency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conference_data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{CalendarEvent, CalendarEventCreateRequest, EventAttendee, EventDateTime};

    #[test]
    fn event_uses_google_ical_uid_field_spelling() {
        let event: CalendarEvent =
            serde_json::from_value(json!({"id": "event-1", "iCalUID": "ical@example.com"}))
                .expect("event");

        assert_eq!(event.i_cal_uid.as_deref(), Some("ical@example.com"));

        let encoded = serde_json::to_value(&event).expect("json");

        assert_eq!(encoded.get("iCalUID"), Some(&json!("ical@example.com")));
        assert!(!encoded.as_object().expect("object").contains_key("iCalUid"));
    }

    #[test]
    fn attendee_preserves_async_operation_and_unknown_fields() {
        let attendee: EventAttendee = serde_json::from_value(json!({
            "email": "ann@example.com",
            "asyncOperation": "move",
            "futureNested": {
                "id": "future-1"
            }
        }))
        .expect("attendee");

        assert_eq!(attendee.async_operation.as_deref(), Some("move"));
        assert_eq!(
            attendee.extra.get("futureNested"),
            Some(&json!({"id": "future-1"}))
        );

        let encoded = serde_json::to_value(&attendee).expect("json");

        assert_eq!(encoded.get("asyncOperation"), Some(&json!("move")));
        assert_eq!(
            encoded.get("futureNested"),
            Some(&json!({"id": "future-1"}))
        );
    }

    #[test]
    fn create_request_omits_unset_optional_fields() {
        let request = CalendarEventCreateRequest {
            id: Some("locality-event-1".to_string()),
            summary: "Locality invite test".to_string(),
            description: Some("Testing Calendar invite creation from Locality.".to_string()),
            location: Some("Test location".to_string()),
            start: EventDateTime {
                date: None,
                date_time: Some("2026-07-21T15:00:00+03:00".to_string()),
                time_zone: Some("Africa/Cairo".to_string()),
            },
            end: EventDateTime {
                date: None,
                date_time: Some("2026-07-21T15:30:00+03:00".to_string()),
                time_zone: Some("Africa/Cairo".to_string()),
            },
            attendees: vec![EventAttendee {
                email: Some("mohammed18200118@gmail.com".to_string()),
                ..EventAttendee::default()
            }],
            conference_data: Some(json!({
                "createRequest": {
                    "requestId": "conference-request-1",
                    "conferenceSolutionKey": { "type": "hangoutsMeet" }
                }
            })),
            extended_properties: Some(json!({
                "private": {
                    "locality_event_id": "locality-event-1"
                }
            })),
            ..CalendarEventCreateRequest::default()
        };

        let encoded = serde_json::to_value(&request).expect("json");

        assert_eq!(
            encoded,
            json!({
                "id": "locality-event-1",
                "summary": "Locality invite test",
                "description": "Testing Calendar invite creation from Locality.",
                "location": "Test location",
                "start": {
                    "dateTime": "2026-07-21T15:00:00+03:00",
                    "timeZone": "Africa/Cairo"
                },
                "end": {
                    "dateTime": "2026-07-21T15:30:00+03:00",
                    "timeZone": "Africa/Cairo"
                },
                "attendees": [
                    {
                        "email": "mohammed18200118@gmail.com"
                    }
                ],
                "conferenceData": {
                    "createRequest": {
                        "requestId": "conference-request-1",
                        "conferenceSolutionKey": { "type": "hangoutsMeet" }
                    }
                },
                "extendedProperties": {
                    "private": {
                        "locality_event_id": "locality-event-1"
                    }
                }
            })
        );
    }
}
