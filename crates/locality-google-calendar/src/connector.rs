use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, ChildContainer,
    Connector, ConnectorCapabilities, ConnectorKind, EnumerateRequest, FetchRequest,
    ListChildrenRequest, ListChildrenResult, NativeEntity, ObserveRequest, ParsedEntity,
};
use locality_core::freshness::{RemoteObservation, RemoteVersion};
use locality_core::journal::{JournalApplyEffect, PushId, PushOperationId};
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::{PropertyValue, PushOperation, PushOperationKind};
use locality_core::validation::ValidationIssue;
use locality_core::{LocalityError, LocalityResult};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::client::{GoogleCalendarApi, HttpGoogleCalendarApiClient};
use crate::dto::{CalendarEvent, CalendarEventCreateRequest, EventAttendee, EventDateTime};
use crate::oauth::GOOGLE_CALENDAR_CONNECTOR_ID;
use crate::render::{
    GOOGLE_CALENDAR_EVENT_NATIVE_KIND, GoogleCalendarDraftDocument,
    parse_google_calendar_draft_document, render_google_calendar_event,
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

        let Some((calendar_id, event_id)) = parse_event_remote_id(&request.remote_id) else {
            return Err(LocalityError::RemoteNotFound(format!(
                "unknown google calendar remote id `{}`",
                request.remote_id.as_str()
            )));
        };
        require_primary_calendar_id(calendar_id)?;
        let event = self.api.get_event(PRIMARY_CALENDAR_ID, event_id)?;
        let deleted = event.status.as_deref() == Some("cancelled");
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
        .deleted(deleted)
        .with_raw_metadata_json(serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string())))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        let Some((calendar_id, event_id)) = parse_event_remote_id(&request.remote_id) else {
            return Err(LocalityError::Unsupported(
                "google calendar fetch remote id",
            ));
        };
        require_primary_calendar_id(calendar_id)?;
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

    fn apply(&self, request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        let mut changed_remote_ids = Vec::new();
        let mut effects = Vec::new();

        for (index, operation) in request.plan.operations.iter().enumerate() {
            let operation_id =
                request.operation_ids.get(index).cloned().ok_or_else(|| {
                    LocalityError::InvalidState("missing operation id".to_string())
                })?;
            let PushOperation::CreateEntity {
                parent_id,
                parent_kind,
                parent_workspace,
                title,
                properties,
                body,
                source_path,
            } = operation
            else {
                return Err(LocalityError::Unsupported("google calendar push operation"));
            };
            if parent_id.as_str() != DRAFT_FOLDER_ID
                || parent_kind.as_ref() != Some(&EntityKind::Directory)
                || *parent_workspace
            {
                return Err(LocalityError::Unsupported("google calendar create parent"));
            }
            if !is_direct_draft_child(source_path) {
                return Err(LocalityError::Unsupported(
                    "google calendar draft source path",
                ));
            }

            let event_id = locality_event_id(request.push_id, &operation_id);
            match self.api.get_event(PRIMARY_CALENDAR_ID, &event_id) {
                Ok(existing) => {
                    let created_id = event_id_or_fallback_string(existing.id.as_deref(), &event_id);
                    let entity_id = event_remote_id(PRIMARY_CALENDAR_ID, &created_id);
                    changed_remote_ids.push(entity_id.clone());
                    effects.push(JournalApplyEffect::CreatedEntity {
                        operation_id,
                        operation_index: index,
                        parent_id: RemoteId::new(EVENTS_FOLDER_ID),
                        entity_id,
                    });
                    continue;
                }
                Err(LocalityError::RemoteNotFound(_)) => {}
                Err(error) => return Err(error),
            }

            let draft = draft_from_push_create(title, properties, body, source_path)?;
            let create_google_meet = draft.create_google_meet;
            let create_request = draft.into_create_request(event_id.clone(), event_id.clone());
            let created =
                self.api
                    .insert_event(PRIMARY_CALENDAR_ID, create_request, create_google_meet)?;
            let created_id = event_id_or_fallback_string(created.id.as_deref(), &event_id);
            let entity_id = event_remote_id(PRIMARY_CALENDAR_ID, &created_id);
            changed_remote_ids.push(entity_id.clone());
            effects.push(JournalApplyEffect::CreatedEntity {
                operation_id,
                operation_index: index,
                parent_id: RemoteId::new(EVENTS_FOLDER_ID),
                entity_id,
            });
        }

        Ok(ApplyPlanResult {
            changed_remote_ids,
            effects,
        })
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

fn require_primary_calendar_id(calendar_id: &str) -> LocalityResult<()> {
    if calendar_id == PRIMARY_CALENDAR_ID {
        Ok(())
    } else {
        Err(LocalityError::Unsupported(
            "google calendar non-primary calendar id",
        ))
    }
}

fn is_direct_draft_child(path: &Path) -> bool {
    let mut components = path.components();
    matches!(
        components.next(),
        Some(Component::Normal(component)) if component == OsStr::new("draft")
    ) && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
}

fn locality_event_id(push_id: &PushId, operation_id: &PushOperationId) -> String {
    let input = format!("{}:{}", push_id.0, operation_id.0);
    let mut event_id = String::from("loc");
    for byte in input.as_bytes() {
        write!(&mut event_id, "{byte:02x}").expect("write hex event id");
    }
    event_id.truncate(1024);
    event_id
}

fn event_id_or_fallback_string(value: Option<&str>, fallback: &str) -> String {
    value
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn draft_from_push_create(
    title: &str,
    properties: &BTreeMap<String, PropertyValue>,
    body: &str,
    source_path: &Path,
) -> LocalityResult<GoogleCalendarDraftDocument> {
    let mut issues = Vec::new();
    let start = event_datetime_property(properties, "start", source_path, &mut issues);
    let end = event_datetime_property(properties, "end", source_path, &mut issues);
    if let (Some(start), Some(end)) = (start.as_ref(), end.as_ref()) {
        match (event_datetime_shape(start), event_datetime_shape(end)) {
            (Some(start_shape), Some(end_shape)) if start_shape != end_shape => {
                issues.push(ValidationIssue::new(
                    "google_calendar_draft_mixed_date_shapes",
                    source_path,
                    Some(1),
                    "Google Calendar draft `start` and `end` must both use `date` or both use `dateTime`",
                    Some(
                        "use `date` for both all-day boundaries or `dateTime` for both timed boundaries"
                            .to_string(),
                    ),
                ));
            }
            _ => {}
        }
    }
    let create_google_meet = google_meet_property(properties, source_path, &mut issues);
    if !issues.is_empty() {
        return Err(LocalityError::Validation(issues));
    }

    Ok(GoogleCalendarDraftDocument {
        summary: string_property(properties, "summary")
            .map(|summary| summary.trim())
            .filter(|summary| !summary.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| title.to_string()),
        description: (!body.is_empty()).then(|| body.to_string()),
        location: non_blank_string_property(properties, "location"),
        start: start.expect("validated start property"),
        end: end.expect("validated end property"),
        attendees: attendees_property(properties),
        recurrence: recurrence_property(properties),
        reminders: properties.get("reminders").map(property_value_to_json),
        transparency: non_blank_string_property(properties, "transparency"),
        visibility: non_blank_string_property(properties, "visibility"),
        create_google_meet,
        extra: BTreeMap::new(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventDateTimeShape {
    Date,
    DateTime,
}

fn event_datetime_property(
    properties: &BTreeMap<String, PropertyValue>,
    field: &str,
    source_path: &Path,
    issues: &mut Vec<ValidationIssue>,
) -> Option<EventDateTime> {
    let Some(value) = properties.get(field) else {
        issues.push(ValidationIssue::new(
            format!("google_calendar_draft_missing_{field}"),
            source_path,
            Some(1),
            format!("Google Calendar draft requires `{field}` frontmatter"),
            Some(format!("add a Google Calendar `{field}` object")),
        ));
        return None;
    };
    let PropertyValue::Object(values) = value else {
        issues.push(invalid_datetime_shape_issue(field, source_path));
        return None;
    };

    let date = validate_datetime_component(field, "date", values.get("date"), source_path, issues);
    let date_time = validate_datetime_component(
        field,
        "dateTime",
        values.get("dateTime"),
        source_path,
        issues,
    );
    if date.invalid || date_time.invalid {
        return None;
    }

    match (date.value, date_time.value) {
        (Some(date), None) => Some(EventDateTime {
            date: Some(date),
            date_time: None,
            time_zone: string_property(values, "timeZone").cloned(),
        }),
        (None, Some(date_time)) => Some(EventDateTime {
            date: None,
            date_time: Some(date_time),
            time_zone: string_property(values, "timeZone").cloned(),
        }),
        _ => {
            issues.push(invalid_datetime_shape_issue(field, source_path));
            None
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DateTimeComponentValidation {
    value: Option<String>,
    invalid: bool,
}

fn validate_datetime_component(
    field: &str,
    component: &str,
    value: Option<&PropertyValue>,
    source_path: &Path,
    issues: &mut Vec<ValidationIssue>,
) -> DateTimeComponentValidation {
    let Some(value) = value else {
        return DateTimeComponentValidation::default();
    };
    match value {
        PropertyValue::String(value) if !value.trim().is_empty() => DateTimeComponentValidation {
            value: Some(value.clone()),
            invalid: false,
        },
        PropertyValue::String(_) => {
            issues.push(ValidationIssue::new(
                format!(
                    "google_calendar_draft_blank_{}_{}",
                    field,
                    date_component_code(component)
                ),
                source_path,
                Some(1),
                format!("Google Calendar draft `{field}.{component}` must not be blank"),
                Some(format!(
                    "remove `{field}.{component}` or set a non-empty {component}"
                )),
            ));
            DateTimeComponentValidation {
                value: None,
                invalid: true,
            }
        }
        _ => {
            issues.push(ValidationIssue::new(
                format!(
                    "google_calendar_draft_invalid_{}_{}",
                    field,
                    date_component_code(component)
                ),
                source_path,
                Some(1),
                format!("Google Calendar draft `{field}.{component}` must be a non-empty string"),
                Some(format!(
                    "remove `{field}.{component}` or set a non-empty string"
                )),
            ));
            DateTimeComponentValidation {
                value: None,
                invalid: true,
            }
        }
    }
}

fn invalid_datetime_shape_issue(field: &str, source_path: &Path) -> ValidationIssue {
    ValidationIssue::new(
        format!("google_calendar_draft_invalid_{field}_shape"),
        source_path,
        Some(1),
        format!("Google Calendar draft `{field}` must include exactly one of `date` or `dateTime`"),
        Some(format!(
            "set either `{field}.date` or `{field}.dateTime`, not both"
        )),
    )
}

fn event_datetime_shape(value: &EventDateTime) -> Option<EventDateTimeShape> {
    match (
        value
            .date
            .as_ref()
            .is_some_and(|date| !date.trim().is_empty()),
        value
            .date_time
            .as_ref()
            .is_some_and(|date_time| !date_time.trim().is_empty()),
    ) {
        (true, false) => Some(EventDateTimeShape::Date),
        (false, true) => Some(EventDateTimeShape::DateTime),
        _ => None,
    }
}

fn date_component_code(component: &str) -> &str {
    match component {
        "dateTime" => "date_time",
        _ => component,
    }
}

fn attendees_property(properties: &BTreeMap<String, PropertyValue>) -> Vec<EventAttendee> {
    match properties.get("attendees") {
        Some(PropertyValue::Array(values)) => values
            .iter()
            .filter_map(|value| match value {
                PropertyValue::Object(values) => attendee_from_properties(values),
                _ => None,
            })
            .collect(),
        Some(PropertyValue::List(values)) => values
            .iter()
            .filter_map(|email| attendee_from_email(email))
            .collect(),
        Some(PropertyValue::String(email)) => attendee_from_email(email).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn attendee_from_email(email: &str) -> Option<EventAttendee> {
    let email = email.trim();
    (!email.is_empty()).then(|| EventAttendee {
        email: Some(email.to_string()),
        ..EventAttendee::default()
    })
}

fn attendee_from_properties(values: &BTreeMap<String, PropertyValue>) -> Option<EventAttendee> {
    let email = non_blank_string_property_from_map(values, "email")?;
    let known = [
        "email",
        "displayName",
        "responseStatus",
        "optional",
        "comment",
        "additionalGuests",
        "resource",
        "id",
    ];
    Some(EventAttendee {
        email: Some(email),
        display_name: non_blank_string_property_from_map(values, "displayName"),
        response_status: non_blank_string_property_from_map(values, "responseStatus"),
        optional: bool_property(values, "optional"),
        comment: non_blank_string_property_from_map(values, "comment"),
        additional_guests: i64_property(values, "additionalGuests"),
        resource: bool_property(values, "resource"),
        id: non_blank_string_property_from_map(values, "id"),
        extra: values
            .iter()
            .filter(|(key, _)| !known.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), property_value_to_json(value)))
            .collect(),
        ..EventAttendee::default()
    })
}

fn recurrence_property(properties: &BTreeMap<String, PropertyValue>) -> Vec<String> {
    match properties.get("recurrence") {
        Some(PropertyValue::List(values)) => values
            .iter()
            .filter_map(|value| non_blank(value).map(ToOwned::to_owned))
            .collect(),
        Some(PropertyValue::Array(values)) => values
            .iter()
            .filter_map(|value| match value {
                PropertyValue::String(value) => non_blank(value).map(ToOwned::to_owned),
                _ => None,
            })
            .collect(),
        Some(PropertyValue::String(value)) => {
            non_blank(value).into_iter().map(str::to_string).collect()
        }
        _ => Vec::new(),
    }
}

fn google_meet_property(
    properties: &BTreeMap<String, PropertyValue>,
    source_path: &Path,
    issues: &mut Vec<ValidationIssue>,
) -> bool {
    let Some(PropertyValue::Object(google_calendar)) = properties.get("google_calendar") else {
        return false;
    };
    let Some(conference) = google_calendar.get("conference") else {
        return false;
    };
    if matches!(conference, PropertyValue::String(value) if value.trim() == "google_meet") {
        return true;
    }

    issues.push(ValidationIssue::new(
        "google_calendar_draft_unsupported_conference",
        source_path,
        Some(1),
        "Google Calendar draft `google_calendar.conference` supports only `google_meet`",
        Some("remove `google_calendar.conference` or set it to `google_meet`".to_string()),
    ));
    false
}

fn property_value_to_json(value: &PropertyValue) -> Value {
    match value {
        PropertyValue::Null => Value::Null,
        PropertyValue::Bool(value) => Value::Bool(*value),
        PropertyValue::Number(value) => value
            .parse::<serde_json::Number>()
            .map(Value::Number)
            .unwrap_or_else(|_| Value::String(value.clone())),
        PropertyValue::String(value) => Value::String(value.clone()),
        PropertyValue::List(values) => Value::Array(
            values
                .iter()
                .map(|value| Value::String(value.clone()))
                .collect(),
        ),
        PropertyValue::Array(values) => {
            Value::Array(values.iter().map(property_value_to_json).collect())
        }
        PropertyValue::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), property_value_to_json(value)))
                .collect::<Map<_, _>>(),
        ),
    }
}

fn string_property<'a>(
    properties: &'a BTreeMap<String, PropertyValue>,
    key: &str,
) -> Option<&'a String> {
    match properties.get(key) {
        Some(PropertyValue::String(value)) => Some(value),
        _ => None,
    }
}

fn non_blank_string_property(
    properties: &BTreeMap<String, PropertyValue>,
    key: &str,
) -> Option<String> {
    non_blank_string_property_from_map(properties, key)
}

fn non_blank_string_property_from_map(
    properties: &BTreeMap<String, PropertyValue>,
    key: &str,
) -> Option<String> {
    string_property(properties, key)
        .and_then(|value| non_blank(value))
        .map(ToOwned::to_owned)
}

fn bool_property(properties: &BTreeMap<String, PropertyValue>, key: &str) -> Option<bool> {
    match properties.get(key) {
        Some(PropertyValue::Bool(value)) => Some(*value),
        _ => None,
    }
}

fn i64_property(properties: &BTreeMap<String, PropertyValue>, key: &str) -> Option<i64> {
    match properties.get(key) {
        Some(PropertyValue::Number(value)) => value.parse().ok(),
        _ => None,
    }
}

fn non_blank(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
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
        ApplyPlanRequest, ChildContainer, Connector, EnumerateRequest, FetchRequest,
        ListChildrenRequest, ObserveRequest,
    };
    use locality_core::LocalityError;
    use locality_core::journal::{JournalApplyEffect, PushId, PushOperationId};
    use locality_core::model::{CanonicalDocument, EntityKind, MountId, RemoteId};
    use locality_core::planner::{PropertyValue, PushOperation, PushOperationKind, PushPlan};
    use locality_core::push::RemotePrecondition;
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
    fn fetch_rejects_non_primary_event_remote_id_without_api_call() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        let connector =
            GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api.clone());

        let error = connector
            .fetch(FetchRequest {
                remote_id: RemoteId::new("google-calendar-event:team-calendar:event-1"),
            })
            .expect_err("non-primary calendar should be rejected");

        assert_eq!(
            error,
            LocalityError::Unsupported("google calendar non-primary calendar id")
        );
        assert!(api.calls.lock().expect("calls").get_events.is_empty());
    }

    #[test]
    fn observe_rejects_non_primary_event_remote_id_without_api_call() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        let connector =
            GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api.clone());

        let error = connector
            .observe(ObserveRequest {
                mount_id: MountId::new("calendar-main"),
                remote_id: RemoteId::new("google-calendar-event:team-calendar:event-1"),
            })
            .expect_err("non-primary calendar should be rejected");

        assert_eq!(
            error,
            LocalityError::Unsupported("google calendar non-primary calendar id")
        );
        assert!(api.calls.lock().expect("calls").get_events.is_empty());
    }

    #[test]
    fn observe_cancelled_event_returns_deleted_observation() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        {
            let mut cancelled = event_fixture("event-1");
            cancelled.status = Some("cancelled".to_string());
            api.calls
                .lock()
                .expect("calls")
                .event_overrides
                .insert("event-1".to_string(), cancelled);
        }
        let connector =
            GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api.clone());

        let observation = connector
            .observe(ObserveRequest {
                mount_id: MountId::new("calendar-main"),
                remote_id: RemoteId::new("google-calendar-event:primary:event-1"),
            })
            .expect("observe");

        assert!(observation.deleted);
        assert_eq!(
            observation.parent_remote_id,
            Some(RemoteId::new("google-calendar-folder:events"))
        );
        assert_eq!(
            observation.projected_path,
            std::path::PathBuf::from("events/20260720-100000-design-review-event-1.md")
        );
        assert!(observation.raw_metadata_json.contains("\"cancelled\""));
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
    fn apply_create_entity_inserts_primary_event_with_send_updates_and_meet() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        let connector =
            GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api.clone());
        let operation_id = PushOperationId("op-1".to_string());
        let plan = draft_create_plan("draft/design-review.md");

        let result = connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("calendar-main"),
                plan: &plan,
                operation_ids: std::slice::from_ref(&operation_id),
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        let created_remote_id = RemoteId::new("google-calendar-event:primary:created-event");
        assert_eq!(result.changed_remote_ids, vec![created_remote_id.clone()]);
        assert_eq!(
            result.effects,
            vec![JournalApplyEffect::CreatedEntity {
                operation_id,
                operation_index: 0,
                parent_id: RemoteId::new("google-calendar-folder:events"),
                entity_id: created_remote_id,
            }]
        );

        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.insert_events.len(), 1);
        let (calendar_id, request, create_conference) = &calls.insert_events[0];
        assert_eq!(calendar_id, "primary");
        assert_eq!(request.summary, "Explicit summary");
        assert_eq!(request.id.as_deref(), Some("loc707573682d313a6f702d31"));
        assert_eq!(request.description.as_deref(), Some("Agenda\n"));
        assert_eq!(
            request.start.date_time.as_deref(),
            Some("2026-07-20T10:00:00-07:00")
        );
        assert_eq!(
            request.end.date_time.as_deref(),
            Some("2026-07-20T10:30:00-07:00")
        );
        assert_eq!(request.attendees.len(), 1);
        assert_eq!(
            request.attendees[0].email.as_deref(),
            Some("ann@example.com")
        );
        assert_eq!(request.attendees[0].optional, Some(true));
        assert_eq!(request.attendees[0].additional_guests, Some(2));
        assert!(request.conference_data.is_some());
        assert!(*create_conference);
    }

    #[test]
    fn apply_create_entity_recovers_existing_event_by_deterministic_id_without_reinsert() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        let connector =
            GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api.clone());
        let push_id = PushId("push-1".to_string());
        let operation_id = PushOperationId("op-1".to_string());
        let event_id = super::locality_event_id(&push_id, &operation_id);
        assert_eq!(event_id, "loc707573682d313a6f702d31");
        {
            let mut calls = api.calls.lock().expect("calls");
            calls
                .event_overrides
                .insert(event_id.clone(), event_fixture("previous-event"));
        }
        let plan = draft_create_plan("draft/design-review.md");

        let result = connector
            .apply(ApplyPlanRequest {
                push_id: &push_id,
                mount_id: &MountId::new("calendar-main"),
                plan: &plan,
                operation_ids: std::slice::from_ref(&operation_id),
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect("apply");

        assert_eq!(
            result.changed_remote_ids,
            vec![RemoteId::new(
                "google-calendar-event:primary:previous-event"
            )]
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.insert_events.len(), 0);
        assert_eq!(calls.get_events, vec![("primary".to_string(), event_id)]);
    }

    #[test]
    fn apply_rejects_nested_draft_source_path() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        let connector =
            GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api.clone());
        let plan = draft_create_plan("draft/nested/design-review.md");

        let error = connector
            .apply(ApplyPlanRequest {
                push_id: &PushId("push-1".to_string()),
                mount_id: &MountId::new("calendar-main"),
                plan: &plan,
                operation_ids: &[PushOperationId("op-1".to_string())],
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect_err("nested draft source should be unsupported");

        assert_eq!(
            error,
            LocalityError::Unsupported("google calendar draft source path")
        );
        let calls = api.calls.lock().expect("calls");
        assert!(calls.get_events.is_empty());
        assert!(calls.insert_events.is_empty());
    }

    #[test]
    fn apply_propagates_get_event_errors_during_idempotency_recovery() {
        let api = Arc::new(FakeGoogleCalendarApi::default());
        let connector =
            GoogleCalendarConnector::with_api(GoogleCalendarConfig::new("token"), api.clone());
        let push_id = PushId("push-1".to_string());
        let operation_id = PushOperationId("op-1".to_string());
        let event_id = super::locality_event_id(&push_id, &operation_id);
        {
            let mut calls = api.calls.lock().expect("calls");
            calls.get_errors.insert(
                event_id.clone(),
                LocalityError::Io("google calendar lookup failed".to_string()),
            );
        }
        let plan = draft_create_plan("draft/design-review.md");

        let error = connector
            .apply(ApplyPlanRequest {
                push_id: &push_id,
                mount_id: &MountId::new("calendar-main"),
                plan: &plan,
                operation_ids: std::slice::from_ref(&operation_id),
                remote_preconditions: &[] as &[RemotePrecondition],
                local_root: None,
            })
            .expect_err("lookup error should propagate");

        assert_eq!(
            error,
            LocalityError::Io("google calendar lookup failed".to_string())
        );
        let calls = api.calls.lock().expect("calls");
        assert_eq!(calls.get_events, vec![("primary".to_string(), event_id)]);
        assert!(calls.insert_events.is_empty());
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

    fn draft_create_plan(source_path: &str) -> PushPlan {
        PushPlan::new(
            vec![RemoteId::new("google-calendar-folder:draft")],
            vec![PushOperation::CreateEntity {
                parent_id: RemoteId::new("google-calendar-folder:draft"),
                parent_kind: Some(EntityKind::Directory),
                parent_workspace: false,
                title: "Design review".to_string(),
                properties: BTreeMap::from([
                    (
                        "summary".to_string(),
                        PropertyValue::String("Explicit summary".to_string()),
                    ),
                    (
                        "start".to_string(),
                        PropertyValue::Object(BTreeMap::from([
                            (
                                "dateTime".to_string(),
                                PropertyValue::String("2026-07-20T10:00:00-07:00".to_string()),
                            ),
                            (
                                "timeZone".to_string(),
                                PropertyValue::String("America/Los_Angeles".to_string()),
                            ),
                        ])),
                    ),
                    (
                        "end".to_string(),
                        PropertyValue::Object(BTreeMap::from([
                            (
                                "dateTime".to_string(),
                                PropertyValue::String("2026-07-20T10:30:00-07:00".to_string()),
                            ),
                            (
                                "timeZone".to_string(),
                                PropertyValue::String("America/Los_Angeles".to_string()),
                            ),
                        ])),
                    ),
                    (
                        "google_calendar".to_string(),
                        PropertyValue::Object(BTreeMap::from([(
                            "conference".to_string(),
                            PropertyValue::String("google_meet".to_string()),
                        )])),
                    ),
                    (
                        "attendees".to_string(),
                        PropertyValue::Array(vec![PropertyValue::Object(BTreeMap::from([
                            (
                                "email".to_string(),
                                PropertyValue::String("ann@example.com".to_string()),
                            ),
                            ("optional".to_string(), PropertyValue::Bool(true)),
                            (
                                "additionalGuests".to_string(),
                                PropertyValue::Number("2".to_string()),
                            ),
                        ]))]),
                    ),
                ]),
                body: "Agenda\n".to_string(),
                source_path: source_path.into(),
            }],
        )
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
        event_overrides: BTreeMap<String, CalendarEvent>,
        get_errors: BTreeMap<String, LocalityError>,
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
                event_overrides: BTreeMap::new(),
                get_errors: BTreeMap::new(),
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
            let mut calls = self.calls.lock().expect("calls");
            calls
                .get_events
                .push((calendar_id.to_string(), event_id.to_string()));
            if let Some(error) = calls.get_errors.get(event_id).cloned() {
                return Err(error);
            }
            if let Some(event) = calls.event_overrides.get(event_id).cloned() {
                return Ok(event);
            }
            if event_id.starts_with("loc") {
                return Err(LocalityError::RemoteNotFound(format!(
                    "google calendar event `{event_id}` not found"
                )));
            }
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
