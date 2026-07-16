use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, ChildContainer,
    Connector, ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest,
    ListChildrenRequest, ListChildrenResult, NativeEntity, ObserveRequest, ParsedEntity,
};
use locality_core::freshness::{RemoteObservation, RemoteVersion};
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::PushOperationKind;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};

use crate::client::{GoogleCalendarApi, HttpGoogleCalendarApiClient};
use crate::dto::{CalendarEvent, CalendarEventCreateRequest};
use crate::oauth::GOOGLE_CALENDAR_CONNECTOR_ID;
use crate::render::{
    GOOGLE_CALENDAR_EVENT_NATIVE_KIND, parse_google_calendar_draft_document,
    render_google_calendar_event,
};
use crate::settings::GoogleCalendarMountSettings;

pub const PRIMARY_CALENDAR_ID: &str = "primary";
const EVENTS_FOLDER_ID: &str = "google-calendar-folder:events";
const DRAFT_FOLDER_ID: &str = "google-calendar-folder:draft";
const LOCAL_DRAFT_REMOTE_ID: &str = "google-calendar-draft:local";
const GOOGLE_CALENDAR_DRAFT_NATIVE_KIND: &str = "google_calendar_draft";
const LOCAL_DRAFT_EVENT_ID: &str = "locality-google-calendar-draft-local";
const LOCAL_DRAFT_CONFERENCE_REQUEST_ID: &str = "locality-google-calendar-conference-local";

#[derive(Clone, PartialEq, Eq)]
pub struct GoogleCalendarConfig {
    pub access_token: String,
    pub settings: GoogleCalendarMountSettings,
}

impl fmt::Debug for GoogleCalendarConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleCalendarConfig")
            .field("access_token", &"<redacted>")
            .field("settings", &self.settings)
            .finish()
    }
}

#[derive(Clone)]
pub struct GoogleCalendarConnector {
    config: GoogleCalendarConfig,
    api: Arc<dyn GoogleCalendarApi>,
}

impl fmt::Debug for GoogleCalendarConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleCalendarConnector")
            .field("access_token", &"<redacted>")
            .field("settings", &self.config.settings)
            .finish()
    }
}

impl GoogleCalendarConfig {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            settings: GoogleCalendarMountSettings::default(),
        }
    }

    pub fn with_settings(mut self, settings: GoogleCalendarMountSettings) -> Self {
        self.settings = settings;
        self
    }
}

impl GoogleCalendarConnector {
    pub fn new(config: GoogleCalendarConfig) -> Self {
        let api = Arc::new(HttpGoogleCalendarApiClient::new(
            config.access_token.clone(),
        ));
        Self::with_api(config, api)
    }

    pub fn with_api(config: GoogleCalendarConfig, api: Arc<dyn GoogleCalendarApi>) -> Self {
        Self { config, api }
    }

    pub fn config(&self) -> &GoogleCalendarConfig {
        &self.config
    }

    pub fn api(&self) -> &dyn GoogleCalendarApi {
        self.api.as_ref()
    }
}

impl Connector for GoogleCalendarConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind(GOOGLE_CALENDAR_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: false,
            supports_databases: false,
            supports_oauth: true,
            supports_remote_observation: true,
            supports_lazy_child_enumeration: true,
            supports_media_download: false,
            supports_undo: false,
            supports_batch_observation: false,
        }
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        [PushOperationKind::CreateEntity].into_iter().collect()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        let mut entries = google_calendar_folder_entries(&request.mount_id, Path::new(""));
        entries.extend(list_primary_event_entries(
            self.api.as_ref(),
            &self.config.settings,
            &request.mount_id,
            Path::new("events"),
        )?);
        Ok(entries)
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let entries = match request.container {
            ChildContainer::Root => {
                google_calendar_folder_entries(&request.mount_id, &request.parent_path)
            }
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == EVENTS_FOLDER_ID =>
            {
                list_primary_event_entries(
                    self.api.as_ref(),
                    &self.config.settings,
                    &request.mount_id,
                    &request.parent_path,
                )?
            }
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == DRAFT_FOLDER_ID =>
            {
                Vec::new()
            }
            _ => Vec::new(),
        };
        Ok(ListChildrenResult::complete(entries))
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        if let Some(folder) = folder_spec(request.remote_id.as_str()) {
            return Ok(folder_observation(
                request.mount_id,
                &request.remote_id,
                folder,
            ));
        }

        let Some((_calendar_id, event_id)) = parse_event_remote_id(&request.remote_id) else {
            return Err(LocalityError::RemoteNotFound(format!(
                "unknown google calendar remote id `{}`",
                request.remote_id.as_str()
            )));
        };
        let event = self.api.get_event(PRIMARY_CALENDAR_ID, event_id)?;
        let entry = event_entry(
            &request.mount_id,
            Path::new("events"),
            PRIMARY_CALENDAR_ID,
            event.clone(),
        );
        Ok(RemoteObservation::new(
            request.mount_id,
            request.remote_id,
            EntityKind::Page,
            entry.title,
            entry.path,
        )
        .with_parent(RemoteId::new(EVENTS_FOLDER_ID))
        .with_remote_version(RemoteVersion::new(event.remote_version()))
        .with_raw_metadata_json(serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string())))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        let Some((_calendar_id, event_id)) = parse_event_remote_id(&request.remote_id) else {
            return Err(LocalityError::Unsupported(
                "google calendar fetch remote id",
            ));
        };
        let event = self.api.get_event(PRIMARY_CALENDAR_ID, event_id)?;
        let bundle = GoogleCalendarNativeBundle {
            calendar_id: PRIMARY_CALENDAR_ID.to_string(),
            event,
        };
        let raw = serde_json::to_vec(&bundle).map_err(|error| {
            LocalityError::Io(format!("google calendar native encode failed: {error}"))
        })?;
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: GOOGLE_CALENDAR_EVENT_NATIVE_KIND.to_string(),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        if entity.kind != GOOGLE_CALENDAR_EVENT_NATIVE_KIND {
            return Err(LocalityError::Unsupported("google calendar native kind"));
        }
        let bundle =
            serde_json::from_slice::<GoogleCalendarNativeBundle>(&entity.raw).map_err(|error| {
                LocalityError::Io(format!("google calendar native decode failed: {error}"))
            })?;
        render_google_calendar_event(&bundle.calendar_id, &entity.remote_id, &bundle.event)
            .map(|rendered| rendered.document)
    }

    fn parse(&self, document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        let draft = parse_google_calendar_draft_document(document)?;
        let create_google_meet = draft.create_google_meet;
        let create_request = draft.into_create_request(
            LOCAL_DRAFT_EVENT_ID.to_string(),
            LOCAL_DRAFT_CONFERENCE_REQUEST_ID.to_string(),
        );
        let raw = serde_json::to_vec(&GoogleCalendarDraftNative {
            create_request,
            create_google_meet,
        })
        .map_err(|error| {
            LocalityError::Io(format!("google calendar draft encode failed: {error}"))
        })?;
        let remote_id = RemoteId::new(LOCAL_DRAFT_REMOTE_ID);
        Ok(ParsedEntity {
            remote_id: remote_id.clone(),
            native: NativeEntity {
                remote_id,
                kind: GOOGLE_CALENDAR_DRAFT_NATIVE_KIND.to_string(),
                raw,
            },
        })
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Ok(())
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        Err(LocalityError::Unsupported("google calendar apply"))
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::Unsupported("google calendar undo"))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct GoogleCalendarNativeBundle {
    calendar_id: String,
    event: CalendarEvent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct GoogleCalendarDraftNative {
    create_request: CalendarEventCreateRequest,
    create_google_meet: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FolderSpec {
    id: &'static str,
    title: &'static str,
}

fn folder_specs() -> [FolderSpec; 2] {
    [
        FolderSpec {
            id: EVENTS_FOLDER_ID,
            title: "events",
        },
        FolderSpec {
            id: DRAFT_FOLDER_ID,
            title: "draft",
        },
    ]
}

fn folder_spec(remote_id: &str) -> Option<FolderSpec> {
    folder_specs()
        .into_iter()
        .find(|folder| folder.id == remote_id)
}

fn google_calendar_folder_entries(mount_id: &MountId, parent_path: &Path) -> Vec<TreeEntry> {
    folder_specs()
        .into_iter()
        .map(|folder| TreeEntry {
            mount_id: mount_id.clone(),
            remote_id: RemoteId::new(folder.id),
            kind: EntityKind::Directory,
            title: folder.title.to_string(),
            path: parent_path.join(folder.title),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: Some(format!("folder:{}", folder.title)),
            stub_frontmatter: None,
        })
        .collect()
}

fn folder_observation(
    mount_id: MountId,
    remote_id: &RemoteId,
    folder: FolderSpec,
) -> RemoteObservation {
    RemoteObservation::new(
        mount_id,
        remote_id.clone(),
        EntityKind::Directory,
        folder.title,
        PathBuf::from(folder.title),
    )
    .with_remote_version(RemoteVersion::new(format!("folder:{}", folder.title)))
    .with_raw_metadata_json(format!(
        r#"{{"kind":"google_calendar_folder","id":"{}","title":"{}"}}"#,
        folder.id, folder.title
    ))
}

fn list_primary_event_entries(
    api: &dyn GoogleCalendarApi,
    settings: &GoogleCalendarMountSettings,
    mount_id: &MountId,
    parent_path: &Path,
) -> LocalityResult<Vec<TreeEntry>> {
    let window = settings.effective_date_window();
    let time_min = window.time_min_rfc3339();
    let time_max = window.time_max_rfc3339();
    let mut page_token = None;
    let mut seen_page_tokens = BTreeSet::new();
    let mut entries = Vec::new();

    loop {
        let page = api.list_events(
            PRIMARY_CALENDAR_ID,
            &time_min,
            &time_max,
            page_token.as_deref(),
        )?;
        entries.extend(
            page.items
                .into_iter()
                .filter(|event| event.status.as_deref() != Some("cancelled"))
                .map(|event| event_entry(mount_id, parent_path, PRIMARY_CALENDAR_ID, event)),
        );

        let Some(next_page_token) = page.next_page_token else {
            break;
        };
        track_next_page_token(&mut seen_page_tokens, &next_page_token)?;
        page_token = Some(next_page_token);
    }

    Ok(entries)
}

fn track_next_page_token(
    seen_page_tokens: &mut BTreeSet<String>,
    next_page_token: &str,
) -> LocalityResult<()> {
    match seen_page_tokens.insert(next_page_token.to_string()) {
        true => Ok(()),
        false => Err(LocalityError::Io(format!(
            "google calendar pagination returned repeated page token `{next_page_token}` for calendar `{PRIMARY_CALENDAR_ID}`"
        ))),
    }
}

fn event_entry(
    mount_id: &MountId,
    parent_path: &Path,
    calendar_id: &str,
    event: CalendarEvent,
) -> TreeEntry {
    let event_id = event_id_or_fallback(&event);
    let remote_id = event_remote_id(calendar_id, event_id);
    let title = event.title();
    let remote_edited_at = Some(event.remote_version());
    let path = parent_path.join(event_filename(&event, event_id));
    let stub_frontmatter = render_google_calendar_event(calendar_id, &remote_id, &event)
        .ok()
        .map(|rendered| rendered.document.frontmatter);

    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id,
        kind: EntityKind::Page,
        title,
        path,
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at,
        stub_frontmatter,
    }
}

fn event_remote_id(calendar_id: &str, event_id: &str) -> RemoteId {
    RemoteId::new(format!("google-calendar-event:{calendar_id}:{event_id}"))
}

fn parse_event_remote_id(remote_id: &RemoteId) -> Option<(&str, &str)> {
    remote_id
        .as_str()
        .strip_prefix("google-calendar-event:")
        .and_then(|rest| rest.split_once(':'))
        .and_then(|(calendar_id, event_id)| {
            (!calendar_id.is_empty() && !event_id.is_empty()).then_some((calendar_id, event_id))
        })
}

fn event_id_or_fallback(event: &CalendarEvent) -> &str {
    event
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or("missing-event-id")
}

fn event_filename(event: &CalendarEvent, event_id: &str) -> String {
    let title = event
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("untitled");
    format!(
        "{}-{}-{}.md",
        compact_start_prefix(&event.sort_start_key()),
        safe_slug(title),
        safe_slug(event_id)
    )
}

fn compact_start_prefix(sort_start_key: &str) -> String {
    let mut digits = sort_start_key
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .take(14)
        .collect::<String>();
    while digits.len() < 14 {
        digits.push('0');
    }
    format!("{}-{}", &digits[..8], &digits[8..14])
}

fn safe_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "untitled".to_string()
    } else {
        slug.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use locality_connector::{
        ChildContainer, Connector, EnumerateRequest, FetchRequest, ListChildrenRequest,
        ObserveRequest,
    };
    use locality_core::LocalityError;
    use locality_core::model::{CanonicalDocument, MountId, RemoteId};
    use locality_core::planner::PushOperationKind;
    use serde_json::json;

    use super::{GoogleCalendarConfig, GoogleCalendarConnector};
    use crate::client::GoogleCalendarApi;
    use crate::dto::{CalendarEvent, CalendarEventCreateRequest, CalendarEventList, EventDateTime};
    use crate::settings::GoogleCalendarMountSettings;

    #[test]
    fn enumerate_projects_events_draft_and_primary_events_only() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        let settings = GoogleCalendarMountSettings::with_date_window("2026-07-01", "2026-07-31")
            .expect("settings");
        let connector = GoogleCalendarConnector::with_api(
            GoogleCalendarConfig::new("token").with_settings(settings),
            api.clone(),
        );

        let entries = connector
            .enumerate(EnumerateRequest {
                mount_id: MountId::new("calendar-main"),
                cursor: None,
            })
            .expect("enumerate");

        let paths = entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        assert!(paths.contains(&std::path::PathBuf::from("events")));
        assert!(paths.contains(&std::path::PathBuf::from("draft")));
        assert!(paths.contains(&std::path::PathBuf::from(
            "events/20260720-100000-design-review-event-1.md"
        )));
        assert!(
            !paths
                .iter()
                .any(|path| path.starts_with("draft") && path.components().count() > 1)
        );

        let calls = api.calls.lock().expect("calls");
        assert_eq!(
            calls.list_events,
            vec![ListEventsCall {
                calendar_id: "primary".to_string(),
                time_min: "2026-07-01T00:00:00Z".to_string(),
                time_max: "2026-07-31T00:00:00Z".to_string(),
                page_token: None,
            }]
        );
    }

    #[test]
    fn list_children_for_draft_folder_returns_no_remote_entries() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        let connector = GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api);

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("calendar-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new(
                    "google-calendar-folder:draft",
                )),
                parent_path: "draft".into(),
            })
            .expect("list draft");

        assert!(result.entries.is_empty());
        assert!(result.is_complete());
    }

    #[test]
    fn list_children_for_root_returns_only_events_and_draft_folders() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        let connector =
            GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api.clone());

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("calendar-main"),
                container: ChildContainer::Root,
                parent_path: "calendar".into(),
            })
            .expect("list root");

        assert!(result.is_complete());
        assert_eq!(result.entries.len(), 2);
        assert_eq!(
            result
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![
                std::path::PathBuf::from("calendar/events"),
                std::path::PathBuf::from("calendar/draft"),
            ]
        );
        assert!(api.calls.lock().expect("calls").list_events.is_empty());
    }

    #[test]
    fn fetch_render_and_observe_primary_event() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        let connector =
            GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api.clone());
        let remote_id = RemoteId::new("google-calendar-event:primary:event-1");

        let native = connector
            .fetch(FetchRequest {
                remote_id: remote_id.clone(),
            })
            .expect("fetch");
        assert_eq!(native.remote_id, remote_id);
        assert_eq!(native.kind, "google_calendar_event");

        let rendered = connector.render(&native).expect("render");
        assert!(rendered.frontmatter.contains("event_id: \"event-1\""));
        assert_eq!(rendered.body, "Agenda\n");

        let observation = connector
            .observe(ObserveRequest {
                mount_id: MountId::new("calendar-main"),
                remote_id: RemoteId::new("google-calendar-event:primary:event-1"),
            })
            .expect("observe");

        assert_eq!(
            observation.parent_remote_id,
            Some(RemoteId::new("google-calendar-folder:events"))
        );
        assert_eq!(observation.title, "Design review");
        assert_eq!(
            observation.projected_path,
            std::path::PathBuf::from("events/20260720-100000-design-review-event-1.md")
        );
        assert!(observation.raw_metadata_json.contains("\"event-1\""));

        let calls = api.calls.lock().expect("calls");
        assert_eq!(
            calls.get_events,
            vec![
                ("primary".to_string(), "event-1".to_string()),
                ("primary".to_string(), "event-1".to_string())
            ]
        );
    }

    #[test]
    fn pagination_follows_tokens_and_skips_cancelled_events() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.pages.insert(
                None,
                CalendarEventList {
                    items: vec![
                        event_fixture("event-1"),
                        CalendarEvent {
                            id: Some("cancelled-1".to_string()),
                            status: Some("cancelled".to_string()),
                            ..event_fixture("cancelled-1")
                        },
                    ],
                    next_page_token: Some("page-2".to_string()),
                    next_sync_token: None,
                },
            );
            calls.pages.insert(
                Some("page-2".to_string()),
                CalendarEventList {
                    items: vec![event_fixture("event-2")],
                    next_page_token: None,
                    next_sync_token: None,
                },
            );
        }
        let settings = GoogleCalendarMountSettings::with_date_window("2026-07-01", "2026-07-31")
            .expect("settings");
        let connector = GoogleCalendarConnector::with_api(
            GoogleCalendarConfig::new("token").with_settings(settings),
            api.clone(),
        );

        let result = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("calendar-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new(
                    "google-calendar-folder:events",
                )),
                parent_path: "events".into(),
            })
            .expect("list events");

        assert!(result.entries.iter().any(|entry| {
            entry.remote_id == RemoteId::new("google-calendar-event:primary:event-1")
        }));
        assert!(result.entries.iter().any(|entry| {
            entry.remote_id == RemoteId::new("google-calendar-event:primary:event-2")
        }));
        assert!(!result.entries.iter().any(|entry| {
            entry.remote_id == RemoteId::new("google-calendar-event:primary:cancelled-1")
        }));
        let calls = api.calls.lock().expect("calls");
        assert_eq!(
            calls
                .list_events
                .iter()
                .map(|call| call.page_token.clone())
                .collect::<Vec<_>>(),
            vec![None, Some("page-2".to_string())]
        );
    }

    #[test]
    fn pagination_detects_repeated_page_token() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.pages.insert(
                None,
                CalendarEventList {
                    items: vec![event_fixture("event-1")],
                    next_page_token: Some("same-token".to_string()),
                    next_sync_token: None,
                },
            );
            calls.pages.insert(
                Some("same-token".to_string()),
                CalendarEventList {
                    items: vec![event_fixture("event-2")],
                    next_page_token: Some("same-token".to_string()),
                    next_sync_token: None,
                },
            );
        }
        let connector =
            GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api.clone());

        let error = connector
            .list_children(ListChildrenRequest {
                mount_id: MountId::new("calendar-main"),
                container: ChildContainer::DirectoryChildren(RemoteId::new(
                    "google-calendar-folder:events",
                )),
                parent_path: "events".into(),
            })
            .expect_err("repeated page token should fail");

        match error {
            LocalityError::Io(message) => {
                assert!(message.contains("repeated page token"));
                assert!(message.contains("same-token"));
            }
            other => panic!("expected io error, got {other:?}"),
        }
    }

    #[test]
    fn parse_returns_google_calendar_draft_native_entity() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        let connector = GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api);

        let parsed = connector
            .parse(&CanonicalDocument::new(
                r#"summary: Design review
start:
  dateTime: "2026-07-20T10:00:00-07:00"
end:
  dateTime: "2026-07-20T10:30:00-07:00"
google_calendar:
  conference: google_meet
"#,
                "Agenda\n",
            ))
            .expect("parse");

        assert_eq!(
            parsed.remote_id,
            RemoteId::new("google-calendar-draft:local")
        );
        assert_eq!(parsed.native.remote_id, parsed.remote_id);
        assert_eq!(parsed.native.kind, "google_calendar_draft");
        let raw: serde_json::Value = serde_json::from_slice(&parsed.native.raw).expect("raw json");
        assert_eq!(
            raw.pointer("/create_request/summary"),
            Some(&json!("Design review"))
        );
        assert_eq!(
            raw.pointer("/create_request/description"),
            Some(&json!("Agenda\n"))
        );
        assert_eq!(raw.pointer("/create_google_meet"), Some(&json!(true)));
    }

    #[test]
    fn capabilities_advertise_oauth_remote_observation_lazy_children_and_create_only() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        let connector = GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api);

        let capabilities = connector.capabilities();

        assert!(!capabilities.supports_block_updates);
        assert!(!capabilities.supports_databases);
        assert!(capabilities.supports_oauth);
        assert!(capabilities.supports_remote_observation);
        assert!(capabilities.supports_lazy_child_enumeration);
        assert!(!capabilities.supports_media_download);
        assert!(!capabilities.supports_undo);
        assert!(!capabilities.supports_batch_observation);
        assert_eq!(
            connector.supported_push_operations(),
            [PushOperationKind::CreateEntity].into_iter().collect()
        );
    }

    #[test]
    fn config_debug_redacts_access_token() {
        let debug = format!("{:?}", GoogleCalendarConfig::new("secret-access-token"));

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-access-token"));
    }

    #[test]
    fn connector_debug_redacts_access_token() {
        let connector = GoogleCalendarConnector::new(GoogleCalendarConfig::new(
            "connector-secret-access-token",
        ));

        let debug = format!("{connector:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("connector-secret-access-token"));
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ListEventsCall {
        calendar_id: String,
        time_min: String,
        time_max: String,
        page_token: Option<String>,
    }

    #[derive(Default, Debug)]
    struct FakeGoogleCalendarApi {
        calls: Mutex<FakeCalls>,
    }

    #[derive(Debug)]
    struct FakeCalls {
        list_events: Vec<ListEventsCall>,
        get_events: Vec<(String, String)>,
        insert_events: Vec<(String, CalendarEventCreateRequest, bool)>,
        pages: BTreeMap<Option<String>, CalendarEventList>,
    }

    impl Default for FakeCalls {
        fn default() -> Self {
            Self {
                list_events: Vec::new(),
                get_events: Vec::new(),
                insert_events: Vec::new(),
                pages: BTreeMap::from([(
                    None,
                    CalendarEventList {
                        items: vec![event_fixture("event-1")],
                        next_page_token: None,
                        next_sync_token: None,
                    },
                )]),
            }
        }
    }

    impl GoogleCalendarApi for FakeGoogleCalendarApi {
        fn list_events(
            &self,
            calendar_id: &str,
            time_min: &str,
            time_max: &str,
            page_token: Option<&str>,
        ) -> locality_core::LocalityResult<CalendarEventList> {
            let mut calls = self.calls.lock().expect("calls");
            calls.list_events.push(ListEventsCall {
                calendar_id: calendar_id.to_string(),
                time_min: time_min.to_string(),
                time_max: time_max.to_string(),
                page_token: page_token.map(str::to_string),
            });
            Ok(calls
                .pages
                .get(&page_token.map(str::to_string))
                .cloned()
                .unwrap_or_default())
        }

        fn get_event(
            &self,
            calendar_id: &str,
            event_id: &str,
        ) -> locality_core::LocalityResult<CalendarEvent> {
            self.calls
                .lock()
                .expect("calls")
                .get_events
                .push((calendar_id.to_string(), event_id.to_string()));
            Ok(event_fixture(event_id))
        }

        fn insert_event(
            &self,
            calendar_id: &str,
            request: CalendarEventCreateRequest,
            create_conference: bool,
        ) -> locality_core::LocalityResult<CalendarEvent> {
            self.calls.lock().expect("calls").insert_events.push((
                calendar_id.to_string(),
                request,
                create_conference,
            ));
            Ok(event_fixture("created-event"))
        }
    }

    fn event_fixture(id: &str) -> CalendarEvent {
        CalendarEvent {
            id: Some(id.to_string()),
            etag: Some(format!("\"{id}-etag\"")),
            status: Some("confirmed".to_string()),
            html_link: Some(format!(
                "https://calendar.google.com/calendar/event?eid={id}"
            )),
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
        }
    }
}
