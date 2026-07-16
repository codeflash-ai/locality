use std::collections::BTreeMap;
use std::path::PathBuf;

use locality_core::model::{CanonicalDocument, RemoteId};
use locality_core::shadow::{ShadowDocument, segment_markdown_body};
use locality_core::validation::ValidationIssue;
use locality_core::{LocalityError, LocalityResult};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::dto::{CalendarEvent, CalendarEventCreateRequest, EventAttendee, EventDateTime};
use crate::oauth::GOOGLE_CALENDAR_CONNECTOR_ID;

pub const GOOGLE_CALENDAR_EVENT_NATIVE_KIND: &str = "google_calendar_event";

#[derive(Clone, Debug, PartialEq)]
pub struct GoogleCalendarRenderedEntity {
    pub document: CanonicalDocument,
    pub shadow: ShadowDocument,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GoogleCalendarDraftDocument {
    pub summary: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: EventDateTime,
    pub end: EventDateTime,
    pub attendees: Vec<EventAttendee>,
    pub recurrence: Vec<String>,
    pub reminders: Option<Value>,
    pub transparency: Option<String>,
    pub visibility: Option<String>,
    pub create_google_meet: bool,
    pub extra: BTreeMap<String, Value>,
}

impl GoogleCalendarDraftDocument {
    pub fn into_create_request(
        self,
        event_id: String,
        conference_request_id: String,
    ) -> CalendarEventCreateRequest {
        let conference_data = self.create_google_meet.then(|| {
            serde_json::json!({
                "createRequest": {
                    "requestId": conference_request_id,
                    "conferenceSolutionKey": { "type": "hangoutsMeet" }
                }
            })
        });
        let mut extended_private = Map::new();
        extended_private.insert(
            "locality_event_id".to_string(),
            Value::String(event_id.clone()),
        );
        let extended_properties = Some(serde_json::json!({
            "private": Value::Object(extended_private)
        }));
        CalendarEventCreateRequest {
            id: Some(event_id),
            summary: self.summary,
            description: self.description,
            location: self.location,
            start: self.start,
            end: self.end,
            attendees: self.attendees,
            recurrence: self.recurrence,
            reminders: self.reminders,
            transparency: self.transparency,
            visibility: self.visibility,
            conference_data,
            extended_properties,
            extra: self.extra,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawDraftFrontmatter {
    title: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    start: Option<EventDateTime>,
    end: Option<EventDateTime>,
    attendees: Option<Vec<EventAttendee>>,
    recurrence: Option<Vec<String>>,
    reminders: Option<Value>,
    transparency: Option<String>,
    visibility: Option<String>,
    google_calendar: Option<RawGoogleCalendarDraftFrontmatter>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
struct RawGoogleCalendarDraftFrontmatter {
    conference: Option<String>,
}

pub fn render_google_calendar_event(
    calendar_id: &str,
    remote_id: &RemoteId,
    event: &CalendarEvent,
) -> LocalityResult<GoogleCalendarRenderedEntity> {
    let body = event.description.clone().unwrap_or_default();
    let frontmatter = event_frontmatter(calendar_id, remote_id, event)?;
    let native_block_ids = segment_markdown_body(&body, 1)
        .into_iter()
        .filter(|block| !block.is_directive())
        .enumerate()
        .map(|(index, _)| RemoteId::new(format!("{}:body:{index}", remote_id.as_str())))
        .collect::<Vec<_>>();
    let shadow =
        ShadowDocument::from_synced_body(remote_id.clone(), body.clone(), 1, native_block_ids)
            .map_err(|error| LocalityError::InvalidState(error.to_string()))?
            .with_frontmatter(frontmatter.clone());
    Ok(GoogleCalendarRenderedEntity {
        document: CanonicalDocument::new(frontmatter, body),
        shadow,
    })
}

pub fn parse_google_calendar_draft_document(
    document: &CanonicalDocument,
) -> LocalityResult<GoogleCalendarDraftDocument> {
    let raw = if document.frontmatter.trim().is_empty() {
        RawDraftFrontmatter::default()
    } else {
        yaml_serde::from_str::<RawDraftFrontmatter>(&document.frontmatter).map_err(|error| {
            LocalityError::Validation(vec![ValidationIssue::new(
                "google_calendar_draft_frontmatter_invalid",
                PathBuf::new(),
                Some(1),
                format!("Google Calendar draft frontmatter is invalid: {error}"),
                Some("fix the YAML frontmatter".to_string()),
            )])
        })?
    };

    let mut issues = Vec::new();
    let summary = raw
        .summary
        .or(raw.title)
        .unwrap_or_default()
        .trim()
        .to_string();
    if summary.is_empty() {
        issues.push(ValidationIssue::new(
            "google_calendar_draft_missing_summary",
            PathBuf::new(),
            Some(1),
            "Google Calendar draft requires `summary` or `title` frontmatter",
            Some("add `summary: \"Event title\"` to the frontmatter".to_string()),
        ));
    }
    let start = match raw.start {
        Some(start) => Some(start),
        None => {
            issues.push(ValidationIssue::new(
                "google_calendar_draft_missing_start",
                PathBuf::new(),
                Some(1),
                "Google Calendar draft requires `start` frontmatter",
                Some("add a Google Calendar `start` object".to_string()),
            ));
            None
        }
    };
    let end = match raw.end {
        Some(end) => Some(end),
        None => {
            issues.push(ValidationIssue::new(
                "google_calendar_draft_missing_end",
                PathBuf::new(),
                Some(1),
                "Google Calendar draft requires `end` frontmatter",
                Some("add a Google Calendar `end` object".to_string()),
            ));
            None
        }
    };
    if let Some(start) = &start
        && !start.is_present()
    {
        issues.push(ValidationIssue::new(
            "google_calendar_draft_empty_start",
            PathBuf::new(),
            Some(1),
            "Google Calendar draft `start` must include `date` or `dateTime`",
            Some("set `start.dateTime` or `start.date`".to_string()),
        ));
    }
    if let Some(end) = &end
        && !end.is_present()
    {
        issues.push(ValidationIssue::new(
            "google_calendar_draft_empty_end",
            PathBuf::new(),
            Some(1),
            "Google Calendar draft `end` must include `date` or `dateTime`",
            Some("set `end.dateTime` or `end.date`".to_string()),
        ));
    }
    if !issues.is_empty() {
        return Err(LocalityError::Validation(issues));
    }
    let start = start.expect("validated start frontmatter");
    let end = end.expect("validated end frontmatter");

    Ok(GoogleCalendarDraftDocument {
        summary,
        description: if document.body.is_empty() {
            raw.description
        } else {
            Some(document.body.clone())
        },
        location: raw.location,
        start,
        end,
        attendees: raw.attendees.unwrap_or_default(),
        recurrence: raw.recurrence.unwrap_or_default(),
        reminders: raw.reminders,
        transparency: raw.transparency,
        visibility: raw.visibility,
        create_google_meet: raw
            .google_calendar
            .and_then(|calendar| calendar.conference)
            .is_some_and(|conference| conference == "google_meet"),
        extra: raw
            .extra
            .into_iter()
            .filter(|(key, _)| !matches!(key.as_str(), "loc" | "google_calendar"))
            .collect(),
    })
}

fn event_frontmatter(
    calendar_id: &str,
    remote_id: &RemoteId,
    event: &CalendarEvent,
) -> LocalityResult<String> {
    let version = event.remote_version();
    let title = event.title();
    let mut root = serde_json::Map::new();
    root.insert(
        "loc".to_string(),
        serde_json::json!({
            "id": remote_id.as_str(),
            "type": "page",
            "connector": GOOGLE_CALENDAR_CONNECTOR_ID,
            "synced_at": version,
            "remote_edited_at": event.remote_version()
        }),
    );
    root.insert("title".to_string(), Value::String(title.clone()));
    root.insert("summary".to_string(), Value::String(title));
    if let Some(start) = &event.start {
        root.insert(
            "start".to_string(),
            serde_json::to_value(start).map_err(json_error)?,
        );
    }
    if let Some(end) = &event.end {
        root.insert(
            "end".to_string(),
            serde_json::to_value(end).map_err(json_error)?,
        );
    }
    if let Some(location) = &event.location {
        root.insert("location".to_string(), Value::String(location.clone()));
    }
    root.insert(
        "google_calendar".to_string(),
        serde_json::json!({
            "calendar_id": calendar_id,
            "event_id": event.id.as_deref().unwrap_or(""),
            "status": event.status.as_deref().unwrap_or(""),
            "html_link": event.html_link.as_deref().unwrap_or(""),
            "event": serde_json::to_value(event).map_err(json_error)?
        }),
    );
    let value = Value::Object(root);
    yaml_serde::to_string(&value)
        .map(|yaml| quote_yaml_frontmatter_strings(yaml.trim_start_matches("---\n")))
        .map_err(|error| {
            LocalityError::Io(format!(
                "google calendar frontmatter encode failed: {error}"
            ))
        })
}

fn json_error(error: serde_json::Error) -> LocalityError {
    LocalityError::Io(format!("google calendar event JSON encode failed: {error}"))
}

fn quote_yaml_frontmatter_strings(yaml: &str) -> String {
    let lines = yaml.lines().collect::<Vec<_>>();
    let mut output = String::with_capacity(yaml.len());
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if let Some((key, value)) = line.split_once(':') {
            let scalar = value.trim();
            let key_name = key.trim().trim_start_matches("- ").trim();
            if matches!(scalar, "|" | "|-" | "|+") {
                let (text, next_index) = collect_yaml_block_scalar(&lines, index);
                output.push_str(key);
                output.push_str(": ");
                output.push_str(&yaml_scalar(&text));
                output.push('\n');
                index = next_index;
                continue;
            }
            if key_name != "connector" && should_quote_yaml_scalar(scalar) {
                output.push_str(key);
                output.push_str(": ");
                output.push_str(&yaml_scalar(scalar));
                output.push('\n');
                index += 1;
                continue;
            }
        }
        output.push_str(line);
        output.push('\n');
        index += 1;
    }
    output
}

fn collect_yaml_block_scalar(lines: &[&str], start: usize) -> (String, usize) {
    let key_indent = leading_space_count(lines[start]);
    let mut index = start + 1;
    let block_indent = lines[index..]
        .iter()
        .find(|line| !line.trim().is_empty())
        .map(|line| leading_space_count(line))
        .unwrap_or(key_indent + 2);
    let mut block_lines = Vec::new();
    while index < lines.len() {
        let line = lines[index];
        if !line.trim().is_empty() && leading_space_count(line) <= key_indent {
            break;
        }
        block_lines.push(line.get(block_indent..).unwrap_or_default());
        index += 1;
    }

    let mut text = block_lines.join("\n");
    if lines[start].trim_end().ends_with('|') || lines[start].trim_end().ends_with("|+") {
        text.push('\n');
    }
    (text, index)
}

fn leading_space_count(line: &str) -> usize {
    line.bytes().take_while(|byte| *byte == b' ').count()
}

fn should_quote_yaml_scalar(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "|" | "|-" | "|+")
        && !matches!(value, "null" | "true" | "false" | "[]" | "{}")
        && !value.starts_with('"')
        && !value.starts_with('\'')
        && !value.starts_with('[')
        && !value.starts_with('{')
        && value.parse::<i64>().is_err()
        && value.parse::<f64>().is_err()
}

fn yaml_scalar(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.push_str(&format!("\\u{:04X}", u32::from(ch))),
            ch => escaped.push(ch),
        }
    }

    format!("\"{}\"", escaped)
}

#[cfg(test)]
mod tests {
    use locality_core::LocalityError;
    use locality_core::model::RemoteId;

    use super::{parse_google_calendar_draft_document, render_google_calendar_event};
    use crate::dto::{CalendarEvent, EventDateTime};

    #[test]
    fn render_event_includes_full_event_yaml_and_uses_description_as_body() {
        let event = CalendarEvent {
            id: Some("event-1".to_string()),
            etag: Some("\"etag-1\"".to_string()),
            status: Some("confirmed".to_string()),
            html_link: Some("https://calendar.google.com/calendar/event?eid=event-1".to_string()),
            updated: Some("2026-07-20T17:30:00Z".to_string()),
            summary: Some("Design review".to_string()),
            description: Some("Agenda\n".to_string()),
            location: Some("Room 12".to_string()),
            start: Some(EventDateTime {
                date: None,
                date_time: Some("2026-07-20T10:00:00-07:00".to_string()),
                time_zone: Some("America/Los_Angeles".to_string()),
            }),
            end: Some(EventDateTime {
                date: None,
                date_time: Some("2026-07-20T10:30:00-07:00".to_string()),
                time_zone: Some("America/Los_Angeles".to_string()),
            }),
            ..CalendarEvent::default()
        };

        let rendered = render_google_calendar_event(
            "primary",
            &RemoteId::new("google-calendar-event:primary:event-1"),
            &event,
        )
        .expect("render");

        assert_eq!(rendered.document.body, "Agenda\n");
        assert!(
            rendered
                .document
                .frontmatter
                .contains("connector: google-calendar")
        );
        assert!(
            rendered
                .document
                .frontmatter
                .contains("summary: \"Design review\"")
        );
        assert!(
            rendered
                .document
                .frontmatter
                .contains("event_id: \"event-1\"")
        );
        assert!(
            rendered
                .document
                .frontmatter
                .contains("html_link: \"https://calendar.google.com/calendar/event?eid=event-1\"")
        );
        assert!(rendered.document.frontmatter.contains("event:"));
        assert!(
            rendered
                .document
                .frontmatter
                .contains("description: \"Agenda\\n\"")
        );
        assert!(
            rendered
                .document
                .frontmatter
                .contains("dateTime: \"2026-07-20T10:00:00-07:00\"")
        );
        assert!(
            rendered
                .shadow
                .frontmatter
                .contains("event_id: \"event-1\"")
        );
    }

    #[test]
    fn parse_draft_accepts_native_timed_event_shape_and_google_meet_flag() {
        let document = locality_core::model::CanonicalDocument::new(
            r#"summary: Design review
location: Room 12
start:
  dateTime: "2026-07-20T10:00:00-07:00"
  timeZone: America/Los_Angeles
end:
  dateTime: "2026-07-20T10:30:00-07:00"
  timeZone: America/Los_Angeles
attendees:
  - email: ann@example.com
google_calendar:
  conference: google_meet
"#,
            "Agenda\n",
        );

        let draft = parse_google_calendar_draft_document(&document).expect("parse draft");

        assert_eq!(draft.summary, "Design review");
        assert_eq!(draft.description.as_deref(), Some("Agenda\n"));
        assert_eq!(draft.location.as_deref(), Some("Room 12"));
        assert_eq!(
            draft.start.date_time.as_deref(),
            Some("2026-07-20T10:00:00-07:00")
        );
        assert_eq!(
            draft.end.date_time.as_deref(),
            Some("2026-07-20T10:30:00-07:00")
        );
        assert_eq!(draft.attendees[0].email.as_deref(), Some("ann@example.com"));
        assert!(draft.create_google_meet);
    }

    #[test]
    fn parse_draft_accepts_all_day_event_shape() {
        let document = locality_core::model::CanonicalDocument::new(
            r#"summary: Offsite
start:
  date: "2026-07-20"
end:
  date: "2026-07-21"
"#,
            "",
        );

        let draft = parse_google_calendar_draft_document(&document).expect("parse draft");

        assert_eq!(draft.summary, "Offsite");
        assert_eq!(draft.start.date.as_deref(), Some("2026-07-20"));
        assert_eq!(draft.end.date.as_deref(), Some("2026-07-21"));
    }

    #[test]
    fn parse_draft_reports_validation_for_missing_summary_start_and_end() {
        let document = locality_core::model::CanonicalDocument::new("location: Room 12\n", "");

        let error = parse_google_calendar_draft_document(&document).expect_err("invalid draft");
        let LocalityError::Validation(issues) = error else {
            panic!("expected validation error");
        };
        let messages = issues
            .iter()
            .map(|issue| issue.message.as_str())
            .collect::<Vec<_>>();

        assert!(
            messages.contains(&"Google Calendar draft requires `summary` or `title` frontmatter")
        );
        assert!(messages.contains(&"Google Calendar draft requires `start` frontmatter"));
        assert!(messages.contains(&"Google Calendar draft requires `end` frontmatter"));
    }
}
