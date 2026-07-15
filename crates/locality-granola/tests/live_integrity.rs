use std::collections::BTreeSet;

use chrono::{DateTime, TimeDelta, Utc};
use locality_core::LocalityError;
use locality_granola::{
    GranolaApi, GranolaContentKind, GranolaNativeBundle, GranolaNote, HttpGranolaApiClient,
    render_granola_note,
};

const LIVE_KEY_ENV: &str = "GRANOLA_API_KEY";
const LIVE_NOTE_ENV: &str = "LOCALITY_GRANOLA_LIVE_NOTE_ID";
const NOTE_SCAN_LIMIT: usize = 12;

#[test]
#[ignore = "requires GRANOLA_API_KEY; reads real Granola notes without mutating them"]
fn live_public_api_paginates_fetches_transcript_and_renders_canonical_files() {
    let api_key = required_secret_env(LIVE_KEY_ENV);
    let api = HttpGranolaApiClient::new(api_key);

    let first_page = api
        .list_notes(None, 30, None, None, None)
        .unwrap_or_else(|error| panic!("Granola list-notes request failed: {error}"));
    assert!(
        !first_page.notes.is_empty(),
        "Granola list-notes returned no accessible notes"
    );
    assert!(
        first_page.notes.len() <= 30,
        "Granola exceeded page_size=30"
    );

    if first_page.has_more {
        let cursor = first_page
            .cursor
            .as_deref()
            .filter(|cursor| !cursor.is_empty())
            .expect("Granola reported has_more without a cursor");
        let second_page = api
            .list_notes(Some(cursor), 30, None, None, None)
            .unwrap_or_else(|error| panic!("Granola cursor pagination failed: {error}"));
        assert!(
            !second_page.notes.is_empty(),
            "Granola cursor returned an empty follow-up page"
        );
        let first_ids = first_page
            .notes
            .iter()
            .map(|note| note.id.as_str())
            .collect::<BTreeSet<_>>();
        assert!(
            second_page
                .notes
                .iter()
                .all(|note| !first_ids.contains(note.id.as_str())),
            "Granola cursor pagination repeated a note from the first page"
        );
    }

    let note = live_note_with_transcript(&api, &first_page.notes);
    assert!(!note.id.trim().is_empty(), "Granola note id was empty");
    assert_eq!(
        note.object, "note",
        "Granola returned an unexpected object type"
    );
    assert!(
        chrono::DateTime::parse_from_rfc3339(&note.created_at).is_ok(),
        "Granola note created_at was not RFC 3339"
    );
    assert!(
        chrono::DateTime::parse_from_rfc3339(&note.updated_at).is_ok(),
        "Granola note updated_at was not RFC 3339"
    );
    assert!(
        note.web_url.starts_with("https://"),
        "Granola note web_url was not HTTPS"
    );

    let metadata_only = api.get_note(&note.id, false).unwrap_or_else(|error| {
        panic!("Granola get-note request without transcript failed: {error}")
    });
    assert_eq!(
        metadata_only.id, note.id,
        "Granola get-note variants returned different note ids"
    );
    assert!(
        metadata_only.title == note.title
            && metadata_only.created_at == note.created_at
            && metadata_only.updated_at == note.updated_at
            && metadata_only.summary_text == note.summary_text
            && metadata_only.summary_markdown == note.summary_markdown,
        "Granola transcript inclusion changed note metadata or summary fields"
    );

    let summary = render_granola_note(&GranolaNativeBundle {
        content_kind: GranolaContentKind::Summary,
        note: note.clone(),
    })
    .expect("render live Granola summary");
    let transcript = render_granola_note(&GranolaNativeBundle {
        content_kind: GranolaContentKind::Transcript,
        note: note.clone(),
    })
    .expect("render live Granola transcript");

    assert_frontmatter_contract(&summary.frontmatter, &note.id, "summary");
    assert_frontmatter_contract(&transcript.frontmatter, &note.id, "transcript");
    assert!(
        !summary.body.trim().is_empty(),
        "rendered Granola summary body was empty"
    );
    let transcript_chunk_count = note.transcript.as_ref().map_or(0, Vec::len);
    assert_compact_transcript_contract(&transcript.body, transcript_chunk_count);

    let note_updated_at = DateTime::parse_from_rfc3339(&note.updated_at)
        .expect("live Granola note updated_at was already validated")
        .with_timezone(&Utc);
    let updated_after = (note_updated_at - TimeDelta::days(2))
        .format("%Y-%m-%d")
        .to_string();
    api.list_notes(None, 1, None, None, Some(&updated_after))
        .unwrap_or_else(|error| {
            panic!("Granola incremental updated_after request failed: {error}")
        });
}

fn live_note_with_transcript(
    api: &HttpGranolaApiClient,
    summaries: &[locality_granola::GranolaNoteSummary],
) -> GranolaNote {
    if let Ok(note_id) = std::env::var(LIVE_NOTE_ENV)
        && !note_id.trim().is_empty()
    {
        match api.get_note(note_id.trim(), true) {
            Ok(note)
                if note
                    .transcript
                    .as_ref()
                    .is_some_and(|chunks| !chunks.is_empty())
                    && has_summary(&note) =>
            {
                return note;
            }
            Ok(_) | Err(LocalityError::RemoteNotFound(_)) => {}
            Err(error) => panic!("Configured Granola live note could not be fetched: {error}"),
        }
    }

    for summary in summaries.iter().take(NOTE_SCAN_LIMIT) {
        let note = api
            .get_note(&summary.id, true)
            .unwrap_or_else(|error| panic!("Granola transcript fetch failed: {error}"));
        if note
            .transcript
            .as_ref()
            .is_some_and(|chunks| !chunks.is_empty())
            && has_summary(&note)
        {
            return note;
        }
    }
    panic!(
        "None of the first {NOTE_SCAN_LIMIT} Granola notes had both a summary and retained transcript; set {LIVE_NOTE_ENV} to a stable qualifying note"
    );
}

fn has_summary(note: &GranolaNote) -> bool {
    note.summary_markdown
        .as_deref()
        .is_some_and(|summary| !summary.trim().is_empty())
        || !note.summary_text.trim().is_empty()
}

fn assert_frontmatter_contract(frontmatter: &str, note_id: &str, kind: &str) {
    assert!(
        frontmatter.contains("  connector: granola\n"),
        "Granola frontmatter omitted connector identity"
    );
    assert!(
        frontmatter.contains(&format!("  content_kind: {kind}\n")),
        "Granola frontmatter used the wrong content kind"
    );
    assert!(
        frontmatter.contains(note_id),
        "Granola frontmatter omitted the durable note id"
    );
    for field in [
        "web_url",
        "created_at",
        "updated_at",
        "owner",
        "attendees",
        "folders",
    ] {
        assert!(
            frontmatter.contains(&format!("  {field}:")),
            "Granola frontmatter omitted required field `{field}`"
        );
    }
}

fn assert_compact_transcript_contract(body: &str, expected_heading_count: usize) {
    let headings = body
        .lines()
        .filter(|line| line.starts_with("**Me") || line.starts_with("**Them"))
        .collect::<Vec<_>>();
    assert!(
        !headings.is_empty(),
        "rendered Granola transcript had no speaker turns"
    );
    assert_eq!(
        headings.len(),
        expected_heading_count,
        "rendered Granola transcript did not preserve one heading per chunk"
    );
    for heading in headings {
        let heading = heading
            .strip_prefix("**")
            .and_then(|value| value.strip_suffix("**"))
            .expect("Granola transcript heading was not bold Markdown");
        let (speaker, time) = heading
            .split_once(" · ")
            .expect("Granola transcript heading was not speaker-first");
        assert!(
            speaker == "Me"
                || speaker == "Them"
                || (speaker.starts_with("Me (") && speaker.ends_with(')'))
                || (speaker.starts_with("Them (") && speaker.ends_with(')')),
            "Granola transcript heading did not lead with Me or Them"
        );
        let normalized_speaker = speaker.to_ascii_lowercase();
        assert!(
            !normalized_speaker.ends_with(" (microphone)")
                && !normalized_speaker.ends_with(" (speaker)"),
            "Granola transcript heading exposed the repeated capture source"
        );
        let time = time
            .strip_suffix(" UTC")
            .expect("Granola transcript heading omitted UTC");
        for part in time.split('–') {
            assert_compact_time(part);
        }
    }
}

fn assert_compact_time(value: &str) {
    let parts = value.split(':').collect::<Vec<_>>();
    assert_eq!(parts.len(), 3, "Granola transcript time was not HH:MM:SS");
    assert!(
        parts
            .iter()
            .all(|part| part.len() == 2 && part.chars().all(|character| character.is_ascii_digit())),
        "Granola transcript time was not compact numeric HH:MM:SS"
    );
}

fn required_secret_env(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| panic!("set {name} to run the live Granola integrity test"))
}
