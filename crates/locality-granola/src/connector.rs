use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Timelike, Utc};
use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, ChildContainer,
    Connector, ConnectorCapabilities, ConnectorExecutionPolicy, ConnectorKind, EnumerateRequest,
    FetchRequest, ListChildrenRequest, ListChildrenResult, NativeEntity, ObserveRequest,
    ParsedEntity,
};
use locality_core::freshness::{RemoteObservation, RemoteVersion};
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::PushOperationKind;
use locality_core::search::{RAW_SEARCH_METADATA_KEY, SearchMetadata};
use locality_core::{LocalityError, LocalityResult};

use crate::client::{GranolaApi, HttpGranolaApiClient};
use crate::dto::{GranolaNote, GranolaNoteSummary};
use crate::render::{
    GranolaContentKind, GranolaNativeBundle, child_remote_id, note_title, parse_child_remote_id,
    remote_version, render_granola_note,
};

pub const GRANOLA_CONNECTOR_ID: &str = "granola";
const PAGE_SIZE: u32 = 30;

#[derive(Clone, PartialEq, Eq)]
pub struct GranolaConfig {
    pub api_key: String,
    pub updated_after: Option<String>,
    pub execution_policy: ConnectorExecutionPolicy,
}

impl GranolaConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            updated_after: None,
            execution_policy: ConnectorExecutionPolicy::Inline,
        }
    }

    pub fn with_updated_after(mut self, updated_after: impl Into<String>) -> Self {
        self.updated_after = Some(updated_after.into());
        self
    }

    pub fn with_execution_policy(mut self, execution_policy: ConnectorExecutionPolicy) -> Self {
        self.execution_policy = execution_policy;
        self
    }
}

impl fmt::Debug for GranolaConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GranolaConfig")
            .field("api_key", &"<redacted>")
            .field("updated_after", &self.updated_after)
            .field("execution_policy", &self.execution_policy)
            .finish()
    }
}

#[derive(Clone)]
pub struct GranolaConnector {
    config: GranolaConfig,
    api: Arc<dyn GranolaApi>,
}

impl fmt::Debug for GranolaConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GranolaConnector")
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl GranolaConnector {
    pub fn new(config: GranolaConfig) -> Self {
        let api = Arc::new(HttpGranolaApiClient::with_execution_policy(
            config.api_key.clone(),
            config.execution_policy,
        ));
        Self::with_api(config, api)
    }

    pub fn with_api(config: GranolaConfig, api: Arc<dyn GranolaApi>) -> Self {
        Self { config, api }
    }

    pub fn config(&self) -> &GranolaConfig {
        &self.config
    }

    fn all_notes(&self) -> LocalityResult<Vec<GranolaNoteSummary>> {
        let mut notes = Vec::new();
        let mut cursor = None;
        loop {
            let page = self.api.list_notes(
                cursor.as_deref(),
                PAGE_SIZE,
                None,
                None,
                self.config.updated_after.as_deref(),
            )?;
            notes.extend(page.notes);
            if !page.has_more {
                break;
            }
            cursor = page.cursor.filter(|value| !value.is_empty());
            if cursor.is_none() {
                return Err(LocalityError::InvalidState(
                    "Granola API reported more notes without a cursor".to_string(),
                ));
            }
        }
        let mut notes = notes
            .into_iter()
            .map(|note| (note.id.clone(), note))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect::<Vec<_>>();
        notes.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(notes)
    }
}

impl Connector for GranolaConnector {
    fn with_execution_policy(&self, policy: ConnectorExecutionPolicy) -> Self {
        Self::new(self.config.clone().with_execution_policy(policy))
    }

    fn kind(&self) -> ConnectorKind {
        ConnectorKind(GRANOLA_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities::read_only()
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        BTreeSet::new()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        self.all_notes().map(|notes| {
            notes
                .into_iter()
                .map(|note| meeting_entry(&request.mount_id, Path::new(""), &note))
                .collect()
        })
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let complete = !matches!(request.container, ChildContainer::Root)
            || self.config.updated_after.is_none();
        let entries = match request.container {
            ChildContainer::Root => self
                .all_notes()?
                .into_iter()
                .map(|note| meeting_entry(&request.mount_id, &request.parent_path, &note))
                .collect(),
            ChildContainer::DirectoryChildren(remote_id) => {
                content_stub_entries(&request.mount_id, &request.parent_path, remote_id.as_str())
            }
            _ => Vec::new(),
        };
        Ok(if complete {
            ListChildrenResult::complete(entries)
        } else {
            ListChildrenResult::incremental(entries)
        })
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        if let Some((note_id, kind)) = parse_child_remote_id(request.remote_id.as_str()) {
            let note = self
                .api
                .get_note(note_id, kind == GranolaContentKind::Transcript)?;
            let parent_path =
                PathBuf::from(meeting_directory_name(&GranolaNoteSummary::from(&note)));
            let entry = content_entry(&request.mount_id, &parent_path, &note, kind);
            let parent = RemoteId::new(note.id.clone());
            return Ok(observation_from_entry(
                entry,
                Some(parent),
                Some(granola_note_metadata_json(&note, Some(kind))),
            ));
        }
        let note = self.api.get_note(request.remote_id.as_str(), false)?;
        let entry = meeting_entry(
            &request.mount_id,
            Path::new(""),
            &GranolaNoteSummary::from(&note),
        );
        Ok(observation_from_entry(
            entry,
            None,
            Some(granola_note_metadata_json(&note, None)),
        ))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        let Some((note_id, kind)) = parse_child_remote_id(request.remote_id.as_str()) else {
            return Err(LocalityError::Unsupported(
                "Granola meeting directory hydration",
            ));
        };
        let note = self
            .api
            .get_note(note_id, kind == GranolaContentKind::Transcript)?;
        let raw = serde_json::to_vec(&GranolaNativeBundle {
            content_kind: kind,
            note,
        })
        .map_err(|error| LocalityError::Io(format!("Granola native encode failed: {error}")))?;
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: format!("granola_{}", kind.as_str()),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        let bundle = serde_json::from_slice::<GranolaNativeBundle>(&entity.raw)
            .map_err(|error| LocalityError::Io(format!("Granola native decode failed: {error}")))?;
        render_granola_note(&bundle)
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::Unsupported("Granola writes"))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Err(LocalityError::Unsupported("Granola writes"))
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        Err(LocalityError::Unsupported("Granola writes"))
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::Unsupported("Granola undo"))
    }
}

fn meeting_entry(mount_id: &MountId, parent: &Path, note: &GranolaNoteSummary) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(note.id.clone()),
        kind: EntityKind::Directory,
        title: summary_title(note),
        path: parent.join(meeting_directory_name(note)),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(format!("granola:{}:{}", note.id, note.updated_at)),
        stub_frontmatter: None,
    }
}

fn content_stub_entries(mount_id: &MountId, parent: &Path, note_id: &str) -> Vec<TreeEntry> {
    [GranolaContentKind::Summary, GranolaContentKind::Transcript]
        .into_iter()
        .map(|kind| TreeEntry {
            mount_id: mount_id.clone(),
            remote_id: RemoteId::new(child_remote_id(note_id, kind)),
            kind: EntityKind::Asset,
            title: match kind {
                GranolaContentKind::Summary => "Summary".to_string(),
                GranolaContentKind::Transcript => "Transcript".to_string(),
            },
            path: parent.join(format!("{}.md", kind.as_str())),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: None,
            stub_frontmatter: None,
        })
        .collect()
}

fn content_entry(
    mount_id: &MountId,
    parent: &Path,
    note: &GranolaNote,
    kind: GranolaContentKind,
) -> TreeEntry {
    let bundle = GranolaNativeBundle {
        content_kind: kind,
        note: note.clone(),
    };
    let frontmatter = render_granola_note(&bundle).ok().map(|doc| doc.frontmatter);
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(child_remote_id(&note.id, kind)),
        kind: EntityKind::Asset,
        title: match kind {
            GranolaContentKind::Summary => note_title(note),
            GranolaContentKind::Transcript => format!("{} — Transcript", note_title(note)),
        },
        path: parent.join(format!("{}.md", kind.as_str())),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(remote_version(note)),
        stub_frontmatter: frontmatter,
    }
}

fn observation_from_entry(
    entry: TreeEntry,
    parent: Option<RemoteId>,
    raw_metadata_json: Option<String>,
) -> RemoteObservation {
    let mut observation = RemoteObservation::new(
        entry.mount_id,
        entry.remote_id,
        entry.kind,
        entry.title,
        entry.path,
    );
    if let Some(parent) = parent {
        observation = observation.with_parent(parent);
    }
    if let Some(version) = entry.remote_edited_at {
        observation = observation.with_remote_version(RemoteVersion::new(version));
    }
    if let Some(raw_metadata_json) = raw_metadata_json {
        observation = observation.with_raw_metadata_json(raw_metadata_json);
    }
    observation
}

fn granola_note_metadata_json(
    note: &GranolaNote,
    content_kind: Option<GranolaContentKind>,
) -> String {
    let mut value = serde_json::to_value(note).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(object) = &mut value {
        let search_metadata = granola_note_search_metadata(note, content_kind);
        if !search_metadata.is_empty()
            && let Ok(search_value) = serde_json::to_value(search_metadata)
        {
            object.insert(RAW_SEARCH_METADATA_KEY.to_string(), search_value);
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn granola_note_search_metadata(
    note: &GranolaNote,
    content_kind: Option<GranolaContentKind>,
) -> SearchMetadata {
    let mut metadata = SearchMetadata::default();
    metadata.push_metadata_text(&note.id);
    metadata.push_alias(&note.id);
    metadata.push_metadata_text(note_title(note));
    metadata.push_metadata_text(&note.object);
    if let Some(kind) = content_kind {
        metadata.push_metadata_text(kind.as_str());
    }
    metadata.push_metadata_text(&note.created_at);
    metadata.push_metadata_text(&note.updated_at);
    metadata.push_metadata_text(&note.owner.email);
    if let Some(name) = &note.owner.name {
        metadata.push_metadata_text(name);
    }
    for attendee in &note.attendees {
        metadata.push_metadata_text(&attendee.email);
        if let Some(name) = &attendee.name {
            metadata.push_metadata_text(name);
        }
    }
    for folder in &note.folder_membership {
        metadata.push_metadata_text(&folder.id);
        metadata.push_metadata_text(&folder.name);
    }
    if let Some(calendar_event) = &note.calendar_event {
        if let Some(event_title) = &calendar_event.event_title {
            metadata.push_metadata_text(event_title);
        }
        if let Some(organiser) = &calendar_event.organiser {
            metadata.push_metadata_text(organiser);
        }
        if let Some(calendar_event_id) = &calendar_event.calendar_event_id {
            metadata.push_metadata_text(calendar_event_id);
            metadata.push_alias(calendar_event_id);
        }
        if let Some(start) = &calendar_event.scheduled_start_time {
            metadata.push_metadata_text(start);
        }
        if let Some(end) = &calendar_event.scheduled_end_time {
            metadata.push_metadata_text(end);
        }
        for invitee in &calendar_event.invitees {
            metadata.push_metadata_text(&invitee.email);
        }
    }
    metadata.set_source_url(note.web_url.clone());
    metadata
}

pub fn meeting_directory_name(note: &GranolaNoteSummary) -> String {
    let timestamp = DateTime::parse_from_rfc3339(&note.created_at)
        .map(|value| {
            let value = value.with_timezone(&Utc);
            if value.nanosecond() == 0 {
                value.format("%Y-%m-%d %H.%M.%S UTC").to_string()
            } else {
                value.format("%Y-%m-%d %H.%M.%S%.f UTC").to_string()
            }
        })
        .unwrap_or_else(|_| safe_filename(&note.created_at, 40));
    format!(
        "{} — {}",
        safe_filename(&summary_title(note), 160),
        timestamp,
    )
}

fn summary_title(note: &GranolaNoteSummary) -> String {
    note.title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("Untitled meeting")
        .to_string()
}

fn safe_filename(value: &str, byte_limit: usize) -> String {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PendingSeparator {
        None,
        Space,
        Divider,
    }

    let mut name = String::new();
    let mut pending = PendingSeparator::None;
    for character in value.chars() {
        if character.is_control()
            || matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
        {
            pending = PendingSeparator::Divider;
            continue;
        }
        if character.is_whitespace() {
            if pending == PendingSeparator::None {
                pending = PendingSeparator::Space;
            }
            continue;
        }

        let separator = match pending {
            PendingSeparator::None => "",
            PendingSeparator::Space => " ",
            PendingSeparator::Divider => " - ",
        };
        if !name.is_empty() && name.len() + separator.len() + character.len_utf8() > byte_limit {
            break;
        }
        if !name.is_empty() {
            name.push_str(separator);
        }
        name.push(character);
        pending = PendingSeparator::None;
    }

    let name = name.trim_matches([' ', '.', '-']);
    if name.is_empty() {
        "Untitled meeting".to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use locality_connector::{
        ChildContainer, Connector, EnumerateRequest, ListChildrenRequest, ObserveRequest,
    };
    use locality_core::model::{EntityKind, MountId, RemoteId};
    use locality_core::search::RAW_SEARCH_METADATA_KEY;

    use crate::client::GranolaApi;
    use crate::dto::{
        GranolaCalendarEvent, GranolaFolder, GranolaNote, GranolaNoteList, GranolaNoteSummary,
        GranolaUser,
    };

    use super::{GranolaConfig, GranolaConnector};

    #[test]
    fn enumeration_paginates_sorts_and_uses_readable_stable_flat_names() {
        let api = Arc::new(FakeApi::with_notes(vec![
            summary("not_second", "2026-07-14T18:30:00Z", Some("Sync")),
            summary("not_first", "2026-07-14T17:30:00Z", Some("Sync")),
        ]));
        let connector = GranolaConnector::with_api(GranolaConfig::new("secret"), api);
        let entries = connector
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("granola-main"),
                cursor: None,
            })
            .expect("enumerate");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].path.to_string_lossy(),
            "Sync — 2026-07-14 17.30.00 UTC"
        );
        assert_eq!(
            entries[1].path.to_string_lossy(),
            "Sync — 2026-07-14 18.30.00 UTC"
        );
    }

    #[test]
    fn meeting_names_preserve_readable_titles_and_replace_unsafe_path_characters() {
        let note = summary(
            "not_1d3tmYTlCICgjy",
            "2026-07-14T17:30:00Z",
            Some("  Sales / Product: weekly sync?  "),
        );

        assert_eq!(
            super::meeting_directory_name(&note),
            "Sales - Product - weekly sync — 2026-07-14 17.30.00 UTC"
        );
    }

    #[test]
    fn meeting_names_fall_back_for_blank_titles_and_invalid_dates() {
        let note = summary("not_", "date/unknown", Some("  "));

        assert_eq!(
            super::meeting_directory_name(&note),
            "Untitled meeting — date - unknown"
        );
    }

    #[test]
    fn meeting_names_keep_available_fractional_timestamp_precision() {
        let note = summary(
            "not_fractional",
            "2026-07-14T17:30:00.159Z",
            Some("Customer call"),
        );

        assert_eq!(
            super::meeting_directory_name(&note),
            "Customer call — 2026-07-14 17.30.00.159 UTC"
        );
    }

    #[test]
    fn meeting_has_summary_and_transcript_children() {
        let api = Arc::new(FakeApi::default());
        let connector = GranolaConnector::with_api(GranolaConfig::new("secret"), api);
        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("granola-main"),
                container: ChildContainer::DirectoryChildren(locality_core::model::RemoteId::new(
                    "not_1d3tmYTlCICgjy",
                )),
                parent_path: "Sync — 2026-07-14 17.30.00 UTC".into(),
            })
            .expect("children");
        assert_eq!(result.entries[0].path.file_name().unwrap(), "summary.md");
        assert_eq!(result.entries[1].path.file_name().unwrap(), "transcript.md");
        assert!(
            result
                .entries
                .iter()
                .all(|entry| entry.kind == EntityKind::Asset)
        );
        assert!(
            result
                .entries
                .iter()
                .all(|entry| entry.stub_frontmatter.is_none())
        );
        assert!(result.is_complete());
    }

    #[test]
    fn full_root_listing_is_a_complete_snapshot() {
        let api = Arc::new(FakeApi::with_notes(vec![summary(
            "not_recent",
            "2026-07-14T18:30:00Z",
            Some("Sync"),
        )]));
        let connector = GranolaConnector::with_api(GranolaConfig::new("secret"), api);

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("granola-main"),
                container: ChildContainer::Root,
                parent_path: PathBuf::new(),
            })
            .expect("list full root");

        assert!(result.is_complete());
        assert_eq!(result.entries.len(), 1);
    }

    #[test]
    fn incremental_root_listing_passes_filter_and_does_not_authorize_pruning() {
        let api = Arc::new(FakeApi::default());
        let connector = GranolaConnector::with_api(
            GranolaConfig::new("secret").with_updated_after("2026-07-12"),
            api.clone(),
        );

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("granola-main"),
                container: ChildContainer::Root,
                parent_path: PathBuf::new(),
            })
            .expect("list root incrementally");

        assert!(!result.is_complete());
        assert_eq!(
            api.updated_after.lock().unwrap().as_slice(),
            &[Some("2026-07-12".to_string())]
        );
    }

    #[test]
    fn observe_adds_search_metadata_from_note_fields() {
        let api = Arc::new(FakeApi::default().with_note(note()));
        let connector = GranolaConnector::with_api(GranolaConfig::new("secret"), api);

        let observation = connector
            .observe(ObserveRequest {
                mount_id: MountId::new("granola-main"),
                remote_id: RemoteId::new("not_1d3tmYTlCICgjy"),
            })
            .expect("observe note");

        let raw_metadata: serde_json::Value =
            serde_json::from_str(&observation.raw_metadata_json).expect("raw metadata json");
        assert_eq!(
            raw_metadata[RAW_SEARCH_METADATA_KEY]["source_url"],
            serde_json::json!("https://app.granola.ai/notes/not_1d3tmYTlCICgjy")
        );
        assert_eq!(
            raw_metadata[RAW_SEARCH_METADATA_KEY]["aliases"],
            serde_json::json!(["not_1d3tmYTlCICgjy", "cal-1"])
        );
        let search_terms = raw_metadata[RAW_SEARCH_METADATA_KEY]["metadata_text"]
            .as_array()
            .expect("metadata_text");
        assert!(search_terms.contains(&serde_json::json!("Customer call")));
        assert!(search_terms.contains(&serde_json::json!("Ada Lovelace")));
        assert!(search_terms.contains(&serde_json::json!("Deals")));
    }

    #[derive(Debug, Default)]
    struct FakeApi {
        notes: Vec<GranolaNoteSummary>,
        note: Mutex<Option<GranolaNote>>,
        updated_after: Mutex<Vec<Option<String>>>,
    }

    impl FakeApi {
        fn with_notes(notes: Vec<GranolaNoteSummary>) -> Self {
            Self {
                notes,
                note: Mutex::new(None),
                updated_after: Mutex::new(Vec::new()),
            }
        }

        fn with_note(self, note: GranolaNote) -> Self {
            *self.note.lock().unwrap() = Some(note);
            self
        }
    }

    impl GranolaApi for FakeApi {
        fn list_notes(
            &self,
            cursor: Option<&str>,
            _page_size: u32,
            _created_after: Option<&str>,
            _created_before: Option<&str>,
            updated_after: Option<&str>,
        ) -> locality_core::LocalityResult<GranolaNoteList> {
            self.updated_after
                .lock()
                .unwrap()
                .push(updated_after.map(str::to_string));
            let offset = cursor
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            let has_more = self.notes.len() > offset + 1;
            let matching = self.notes.iter().skip(offset).take(1).cloned().collect();
            Ok(GranolaNoteList {
                notes: matching,
                has_more,
                cursor: has_more.then(|| (offset + 1).to_string()),
            })
        }

        fn get_note(
            &self,
            _note_id: &str,
            _include_transcript: bool,
        ) -> locality_core::LocalityResult<GranolaNote> {
            Ok(self.note.lock().unwrap().clone().expect("note"))
        }
    }

    fn summary(id: &str, created_at: &str, title: Option<&str>) -> GranolaNoteSummary {
        GranolaNoteSummary {
            id: id.to_string(),
            object: "note".to_string(),
            title: title.map(str::to_string),
            owner: GranolaUser {
                name: None,
                email: "owner@example.com".to_string(),
            },
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
        }
    }

    fn note() -> GranolaNote {
        GranolaNote {
            id: "not_1d3tmYTlCICgjy".to_string(),
            object: "note".to_string(),
            title: Some("Customer call".to_string()),
            owner: GranolaUser {
                name: Some("Ada Lovelace".to_string()),
                email: "ada@example.com".to_string(),
            },
            created_at: "2026-07-14T17:30:00Z".to_string(),
            updated_at: "2026-07-14T18:00:00Z".to_string(),
            web_url: "https://app.granola.ai/notes/not_1d3tmYTlCICgjy".to_string(),
            calendar_event: Some(GranolaCalendarEvent {
                event_title: Some("Customer discovery".to_string()),
                invitees: Vec::new(),
                organiser: Some("ada@example.com".to_string()),
                calendar_event_id: Some("cal-1".to_string()),
                scheduled_start_time: Some("2026-07-14T17:30:00Z".to_string()),
                scheduled_end_time: Some("2026-07-14T18:00:00Z".to_string()),
            }),
            attendees: vec![GranolaUser {
                name: Some("Grace Hopper".to_string()),
                email: "grace@example.com".to_string(),
            }],
            folder_membership: vec![GranolaFolder {
                id: "folder-1".to_string(),
                object: "folder".to_string(),
                name: "Deals".to_string(),
                parent_folder_id: None,
            }],
            summary_text: "Summary".to_string(),
            summary_markdown: Some("Summary".to_string()),
            transcript: None,
        }
    }
}
