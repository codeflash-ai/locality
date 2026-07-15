use chrono::{DateTime, Utc};
use locality_core::model::CanonicalDocument;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::connector::GRANOLA_CONNECTOR_ID;
use crate::dto::{GranolaNote, GranolaSpeaker};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GranolaContentKind {
    Summary,
    Transcript,
}

impl GranolaContentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Transcript => "transcript",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GranolaNativeBundle {
    pub content_kind: GranolaContentKind,
    pub note: GranolaNote,
}

pub fn render_granola_note(bundle: &GranolaNativeBundle) -> LocalityResult<CanonicalDocument> {
    if bundle.note.id.trim().is_empty() {
        return Err(LocalityError::InvalidState(
            "Granola note is missing its id".to_string(),
        ));
    }
    let body = match bundle.content_kind {
        GranolaContentKind::Summary => render_summary(&bundle.note),
        GranolaContentKind::Transcript => render_transcript(&bundle.note),
    };
    Ok(CanonicalDocument::new(frontmatter(bundle), body))
}

pub fn remote_version(note: &GranolaNote) -> String {
    format!("granola:render-v2:{}:{}", note.id, note.updated_at)
}

pub fn child_remote_id(note_id: &str, kind: GranolaContentKind) -> String {
    format!("{note_id}:{}", kind.as_str())
}

pub fn parse_child_remote_id(remote_id: &str) -> Option<(&str, GranolaContentKind)> {
    if let Some(note_id) = remote_id.strip_suffix(":summary") {
        Some((note_id, GranolaContentKind::Summary))
    } else {
        remote_id
            .strip_suffix(":transcript")
            .map(|note_id| (note_id, GranolaContentKind::Transcript))
    }
}

fn frontmatter(bundle: &GranolaNativeBundle) -> String {
    let note = &bundle.note;
    let title = note_title(note);
    let mut output = format!(
        "loc:\n  id: {}\n  type: page\n  connector: {}\n  synced_at: {}\n  remote_edited_at: {}\ntitle: {}\ngranola:\n  note_id: {}\n  content_kind: {}\n  web_url: {}\n  created_at: {}\n  updated_at: {}\n  owner:\n    name: {}\n    email: {}\n",
        yaml_scalar(&child_remote_id(&note.id, bundle.content_kind)),
        GRANOLA_CONNECTOR_ID,
        yaml_scalar(&note.updated_at),
        yaml_scalar(&note.updated_at),
        yaml_scalar(&title),
        yaml_scalar(&note.id),
        bundle.content_kind.as_str(),
        yaml_scalar(&note.web_url),
        yaml_scalar(&note.created_at),
        yaml_scalar(&note.updated_at),
        yaml_scalar(note.owner.name.as_deref().unwrap_or("")),
        yaml_scalar(&note.owner.email),
    );

    if let Some(calendar) = &note.calendar_event {
        output.push_str("  calendar:\n");
        output.push_str(&format!(
            "    title: {}\n    event_id: {}\n    organizer: {}\n    scheduled_start: {}\n    scheduled_end: {}\n    invitees:\n",
            yaml_scalar(calendar.event_title.as_deref().unwrap_or("")),
            yaml_scalar(calendar.calendar_event_id.as_deref().unwrap_or("")),
            yaml_scalar(calendar.organiser.as_deref().unwrap_or("")),
            yaml_scalar(calendar.scheduled_start_time.as_deref().unwrap_or("")),
            yaml_scalar(calendar.scheduled_end_time.as_deref().unwrap_or("")),
        ));
        if calendar.invitees.is_empty() {
            output.push_str("      []\n");
        } else {
            for invitee in &calendar.invitees {
                output.push_str(&format!("      - {}\n", yaml_scalar(&invitee.email)));
            }
        }
    } else {
        output.push_str("  calendar: null\n");
    }

    output.push_str("  attendees:\n");
    if note.attendees.is_empty() {
        output.push_str("    []\n");
    } else {
        for attendee in &note.attendees {
            output.push_str(&format!(
                "    - name: {}\n      email: {}\n",
                yaml_scalar(attendee.name.as_deref().unwrap_or("")),
                yaml_scalar(&attendee.email),
            ));
        }
    }

    output.push_str("  folders:\n");
    if note.folder_membership.is_empty() {
        output.push_str("    []\n");
    } else {
        for folder in &note.folder_membership {
            output.push_str(&format!(
                "    - id: {}\n      name: {}\n      parent_folder_id: {}\n",
                yaml_scalar(&folder.id),
                yaml_scalar(&folder.name),
                folder
                    .parent_folder_id
                    .as_deref()
                    .map(yaml_scalar)
                    .unwrap_or_else(|| "null".to_string()),
            ));
        }
    }
    output
}

fn render_summary(note: &GranolaNote) -> String {
    if let Some(markdown) = note
        .summary_markdown
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return ensure_trailing_newline(escape_locality_directive_lines(markdown));
    }
    if !note.summary_text.trim().is_empty() {
        return ensure_trailing_newline(escape_locality_directive_lines(&note.summary_text));
    }
    "_No summary is available._\n".to_string()
}

fn render_transcript(note: &GranolaNote) -> String {
    let Some(chunks) = note.transcript.as_ref().filter(|chunks| !chunks.is_empty()) else {
        return "_Granola did not return a transcript. None may have been captured, or it may have been removed by a transcript retention policy._\n".to_string();
    };

    let mut output = String::new();
    for chunk in chunks {
        let speaker = speaker_label(&chunk.speaker);
        let time = transcript_time_range(&chunk.start_time, &chunk.end_time);
        output.push_str(&format!("**{} · {}**\n\n", speaker, time));
        output.push_str(&escape_locality_directive_lines(&chunk.text));
        output.push_str("\n\n");
    }
    output
}

fn speaker_label(speaker: &GranolaSpeaker) -> String {
    let role = if speaker.source.eq_ignore_ascii_case("microphone") {
        "Me"
    } else {
        "Them"
    };
    let detail = speaker
        .name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            speaker
                .diarization_label
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
        .filter(|value| !value.eq_ignore_ascii_case(role));
    detail
        .map(|value| format!("{role} ({})", value.trim()))
        .unwrap_or_else(|| role.to_string())
}

fn transcript_time_range(start: &str, end: &str) -> String {
    let start = compact_transcript_time(start);
    let end = compact_transcript_time(end);
    if start == end {
        format!("{start} UTC")
    } else {
        format!("{start}–{end} UTC")
    }
}

fn compact_transcript_time(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map(|value| value.format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| value.to_string())
}

pub fn note_title(note: &GranolaNote) -> String {
    note.title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Untitled meeting")
        .to_string()
}

fn yaml_scalar(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn escape_locality_directive_lines(value: &str) -> String {
    value
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("::loc{") {
                format!("\\{line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn ensure_trailing_newline(mut value: String) -> String {
    if !value.ends_with('\n') {
        value.push('\n');
    }
    value
}

#[cfg(test)]
mod tests {
    use crate::dto::{GranolaNote, GranolaSpeaker, GranolaTranscriptChunk, GranolaUser};

    use super::{GranolaContentKind, GranolaNativeBundle, remote_version, render_granola_note};

    fn note() -> GranolaNote {
        GranolaNote {
            id: "not_1d3tmYTlCICgjy".to_string(),
            object: "note".to_string(),
            title: Some("Weekly sync".to_string()),
            owner: GranolaUser {
                name: Some("Oat Benson".to_string()),
                email: "oat@example.com".to_string(),
            },
            created_at: "2026-07-14T17:30:00Z".to_string(),
            updated_at: "2026-07-14T18:30:00Z".to_string(),
            web_url: "https://notes.granola.ai/d/example".to_string(),
            calendar_event: None,
            attendees: Vec::new(),
            folder_membership: Vec::new(),
            summary_text: "Fallback".to_string(),
            summary_markdown: Some("## Decisions\n\nShip it.\n".to_string()),
            transcript: Some(vec![GranolaTranscriptChunk {
                speaker: GranolaSpeaker {
                    source: "microphone".to_string(),
                    diarization_label: None,
                    name: Some("Oat Benson".to_string()),
                },
                text: "Hello there.".to_string(),
                start_time: "2026-07-14T17:30:01Z".to_string(),
                end_time: "2026-07-14T17:30:03Z".to_string(),
            }]),
        }
    }

    #[test]
    fn renders_summary_with_exact_markdown_and_metadata() {
        let document = render_granola_note(&GranolaNativeBundle {
            content_kind: GranolaContentKind::Summary,
            note: note(),
        })
        .expect("render");
        assert_eq!(document.body, "## Decisions\n\nShip it.\n");
        assert!(
            document
                .frontmatter
                .contains("id: \"not_1d3tmYTlCICgjy:summary\"")
        );
        assert!(document.frontmatter.contains("calendar: null"));
        assert_eq!(
            remote_version(&note()),
            "granola:render-v2:not_1d3tmYTlCICgjy:2026-07-14T18:30:00Z"
        );
    }

    #[test]
    fn transcript_leads_with_speaker_role_and_compact_time() {
        let document = render_granola_note(&GranolaNativeBundle {
            content_kind: GranolaContentKind::Transcript,
            note: note(),
        })
        .expect("render");
        assert_eq!(
            document.body,
            "**Me (Oat Benson) · 17:30:01–17:30:03 UTC**\n\nHello there.\n\n"
        );
        assert!(!document.body.contains("2026-07-14"));
        assert!(!document.body.contains("microphone"));
    }

    #[test]
    fn transcript_uses_them_without_repeating_the_capture_source() {
        let mut note = note();
        let chunk = note.transcript.as_mut().unwrap().first_mut().unwrap();
        chunk.speaker = GranolaSpeaker {
            source: "speaker".to_string(),
            diarization_label: None,
            name: None,
        };
        chunk.start_time = "2026-07-14T17:30:01.100Z".to_string();
        chunk.end_time = "2026-07-14T17:30:01.900Z".to_string();

        let document = render_granola_note(&GranolaNativeBundle {
            content_kind: GranolaContentKind::Transcript,
            note,
        })
        .expect("render");
        assert_eq!(document.body, "**Them · 17:30:01 UTC**\n\nHello there.\n\n");
    }

    #[test]
    fn missing_transcript_is_a_stable_document() {
        let mut note = note();
        note.transcript = None;
        let document = render_granola_note(&GranolaNativeBundle {
            content_kind: GranolaContentKind::Transcript,
            note,
        })
        .expect("render");
        assert!(document.body.contains("retention policy"));
    }
}
