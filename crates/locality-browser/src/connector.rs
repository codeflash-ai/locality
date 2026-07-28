use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::fs;
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
use locality_core::search::{RAW_SEARCH_METADATA_KEY, SearchMetadata};
use locality_core::{LocalityError, LocalityResult};

use crate::dto::{BrowserSession, BrowserTab, BrowserTabContext};
use crate::render::{
    BrowserNativeBundle, remote_version_for_session, remote_version_for_tab, render_browser_entity,
};

pub const BROWSER_CONNECTOR_ID: &str = "browser";

const SESSIONS_DIRECTORY_NAME: &str = "Sessions";
const SESSION_SUMMARY_FILENAME: &str = "session.md";
const TABS_DIRECTORY_NAME: &str = "tabs";
const PAGE_FILENAME: &str = "page.md";
const SESSIONS_ROOT_REMOTE_ID: &str = "browser:sessions";
const SESSION_REMOTE_ID_PREFIX: &str = "browser:session:";
const SESSION_SUMMARY_REMOTE_ID_PREFIX: &str = "browser:session-summary:";
const SESSION_TABS_REMOTE_ID_PREFIX: &str = "browser:session-tabs:";
const TAB_REMOTE_ID_PREFIX: &str = "browser:tab:";

#[derive(Clone, PartialEq, Eq)]
pub struct BrowserConfig {
    pub capture_root: PathBuf,
}

impl BrowserConfig {
    pub fn new(capture_root: impl Into<PathBuf>) -> Self {
        Self {
            capture_root: capture_root.into(),
        }
    }
}

impl fmt::Debug for BrowserConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrowserConfig")
            .field("capture_root", &self.capture_root)
            .finish()
    }
}

pub trait BrowserCaptureStore: Send + Sync {
    fn capture_root(&self) -> &Path;
    fn list_sessions(&self) -> LocalityResult<Vec<BrowserSession>>;

    fn get_session(&self, session_id: &str) -> LocalityResult<BrowserSession> {
        self.list_sessions()?
            .into_iter()
            .find(|session| session.id == session_id)
            .ok_or_else(|| LocalityError::RemoteNotFound(session_id.to_string()))
    }

    fn get_tab(&self, session_id: &str, tab_id: &str) -> LocalityResult<BrowserTabContext> {
        let session = self.get_session(session_id)?;
        let mut index = 1usize;
        for window in &session.windows {
            for tab in &window.tabs {
                if tab.id == tab_id {
                    return Ok(BrowserTabContext {
                        session: session.clone(),
                        window: window.clone(),
                        tab: tab.clone(),
                        index,
                    });
                }
                index += 1;
            }
        }
        Err(LocalityError::RemoteNotFound(format!(
            "{session_id}:{tab_id}"
        )))
    }
}

#[derive(Clone, Debug)]
pub struct FsBrowserCaptureStore {
    capture_root: PathBuf,
}

impl FsBrowserCaptureStore {
    pub fn new(capture_root: impl Into<PathBuf>) -> Self {
        Self {
            capture_root: capture_root.into(),
        }
    }
}

impl BrowserCaptureStore for FsBrowserCaptureStore {
    fn capture_root(&self) -> &Path {
        &self.capture_root
    }

    fn list_sessions(&self) -> LocalityResult<Vec<BrowserSession>> {
        let sessions_dir = self.capture_root.join("sessions");
        if !sessions_dir.exists() {
            return Ok(Vec::new());
        }
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&sessions_dir).map_err(|error| {
            LocalityError::Io(format!(
                "failed to read browser capture sessions `{}`: {error}",
                sessions_dir.display()
            ))
        })? {
            let entry = entry.map_err(|error| {
                LocalityError::Io(format!(
                    "failed to read browser capture session entry `{}`: {error}",
                    sessions_dir.display()
                ))
            })?;
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("json")
            {
                continue;
            }
            let contents = fs::read_to_string(&path).map_err(|error| {
                LocalityError::Io(format!(
                    "failed to read browser capture session `{}`: {error}",
                    path.display()
                ))
            })?;
            let session = serde_json::from_str::<BrowserSession>(&contents).map_err(|error| {
                LocalityError::Io(format!(
                    "browser capture session `{}` is invalid: {error}",
                    path.display()
                ))
            })?;
            sessions.push(session);
        }
        sessions.sort_by(|left, right| {
            left.captured_at
                .cmp(&right.captured_at)
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(sessions)
    }
}

#[derive(Clone)]
pub struct BrowserConnector {
    config: BrowserConfig,
    store: Arc<dyn BrowserCaptureStore>,
}

impl fmt::Debug for BrowserConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrowserConnector")
            .field("config", &self.config)
            .finish()
    }
}

impl BrowserConnector {
    pub fn new(config: BrowserConfig) -> Self {
        let store = Arc::new(FsBrowserCaptureStore::new(config.capture_root.clone()));
        Self::with_store(config, store)
    }

    pub fn with_store(config: BrowserConfig, store: Arc<dyn BrowserCaptureStore>) -> Self {
        Self { config, store }
    }

    pub fn config(&self) -> &BrowserConfig {
        &self.config
    }
}

impl Connector for BrowserConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind(BROWSER_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities::read_only()
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        BTreeSet::new()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        Ok(entries_for_sessions(
            &request.mount_id,
            Path::new(""),
            self.store.list_sessions()?,
        ))
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let entries = match request.container {
            ChildContainer::Root => {
                vec![sessions_root_entry(&request.mount_id, &request.parent_path)]
            }
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == SESSIONS_ROOT_REMOTE_ID =>
            {
                self.store
                    .list_sessions()?
                    .into_iter()
                    .map(|session| {
                        session_directory_entry(&request.mount_id, &request.parent_path, &session)
                    })
                    .collect()
            }
            ChildContainer::DirectoryChildren(remote_id) => list_browser_directory_children(
                &request.mount_id,
                &request.parent_path,
                &remote_id,
                &*self.store,
            )?,
            _ => Vec::new(),
        };
        Ok(ListChildrenResult::complete(entries))
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        let remote_id = request.remote_id.as_str();
        if remote_id == SESSIONS_ROOT_REMOTE_ID {
            return Ok(observation_from_entry(
                sessions_root_entry(&request.mount_id, Path::new("")),
                None,
                None,
            ));
        }
        if let Some(session_id) = decode_prefixed(remote_id, SESSION_REMOTE_ID_PREFIX) {
            let session = self.store.get_session(&session_id)?;
            return Ok(observation_from_entry(
                session_directory_entry(
                    &request.mount_id,
                    Path::new(SESSIONS_DIRECTORY_NAME),
                    &session,
                ),
                Some(RemoteId::new(SESSIONS_ROOT_REMOTE_ID)),
                Some(session_metadata_json(&session)),
            ));
        }
        if let Some(session_id) = decode_prefixed(remote_id, SESSION_SUMMARY_REMOTE_ID_PREFIX) {
            let session = self.store.get_session(&session_id)?;
            return Ok(observation_from_entry(
                session_summary_entry(&request.mount_id, &session_path(&session), &session),
                Some(RemoteId::new(session_remote_id(&session.id))),
                Some(session_metadata_json(&session)),
            ));
        }
        if let Some(session_id) = decode_prefixed(remote_id, SESSION_TABS_REMOTE_ID_PREFIX) {
            let session = self.store.get_session(&session_id)?;
            return Ok(observation_from_entry(
                session_tabs_entry(&request.mount_id, &session_path(&session), &session),
                Some(RemoteId::new(session_remote_id(&session.id))),
                Some(session_metadata_json(&session)),
            ));
        }
        if let Some((session_id, tab_id)) = decode_tab_remote_id(remote_id) {
            let context = self.store.get_tab(&session_id, &tab_id)?;
            return Ok(observation_from_entry(
                tab_entry(
                    &request.mount_id,
                    &session_path(&context.session).join(TABS_DIRECTORY_NAME),
                    &context.session,
                    &context.tab,
                    context.index,
                ),
                Some(RemoteId::new(session_tabs_remote_id(&context.session.id))),
                Some(tab_metadata_json(&context.session, &context.tab)),
            ));
        }
        Err(LocalityError::Unsupported("Browser observation"))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        let remote_id = request.remote_id.as_str();
        let bundle = if let Some(session_id) =
            decode_prefixed(remote_id, SESSION_SUMMARY_REMOTE_ID_PREFIX)
        {
            BrowserNativeBundle::Session {
                capture_root: self.store.capture_root().to_path_buf(),
                session: self.store.get_session(&session_id)?,
            }
        } else if let Some((session_id, tab_id)) = decode_tab_remote_id(remote_id) {
            let context = self.store.get_tab(&session_id, &tab_id)?;
            BrowserNativeBundle::Tab {
                capture_root: self.store.capture_root().to_path_buf(),
                session: context.session,
                window: context.window,
                tab: context.tab,
                index: context.index,
            }
        } else {
            return Err(LocalityError::Unsupported(
                "Browser directory hydration is not supported",
            ));
        };
        let raw = serde_json::to_vec(&bundle)
            .map_err(|error| LocalityError::Io(format!("Browser native encode failed: {error}")))?;
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: "browser_entity".to_string(),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        let bundle = serde_json::from_slice::<BrowserNativeBundle>(&entity.raw)
            .map_err(|error| LocalityError::Io(format!("Browser native decode failed: {error}")))?;
        render_browser_entity(&bundle)
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::Unsupported("Browser captures are read-only"))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Err(LocalityError::Unsupported("Browser captures are read-only"))
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        Err(LocalityError::Unsupported("Browser captures are read-only"))
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::Unsupported("Browser undo is not supported"))
    }
}

fn entries_for_sessions(
    mount_id: &MountId,
    parent: &Path,
    sessions: Vec<BrowserSession>,
) -> Vec<TreeEntry> {
    let mut entries = vec![sessions_root_entry(mount_id, parent)];
    for session in sessions {
        let session_parent = parent.join(SESSIONS_DIRECTORY_NAME);
        let current_session_path = session_path_with_parent(&session_parent, &session);
        entries.push(session_directory_entry(mount_id, &session_parent, &session));
        entries.push(session_summary_entry(
            mount_id,
            &current_session_path,
            &session,
        ));
        entries.push(session_tabs_entry(
            mount_id,
            &current_session_path,
            &session,
        ));
        entries.extend(tab_entries(
            mount_id,
            &current_session_path.join(TABS_DIRECTORY_NAME),
            &session,
        ));
    }
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.remote_id.cmp(&right.remote_id))
    });
    entries
}

fn list_browser_directory_children(
    mount_id: &MountId,
    parent_path: &Path,
    remote_id: &RemoteId,
    store: &dyn BrowserCaptureStore,
) -> LocalityResult<Vec<TreeEntry>> {
    if let Some(session_id) = decode_prefixed(remote_id.as_str(), SESSION_REMOTE_ID_PREFIX) {
        let session = store.get_session(&session_id)?;
        return Ok(vec![
            session_summary_entry(mount_id, parent_path, &session),
            session_tabs_entry(mount_id, parent_path, &session),
        ]);
    }
    if let Some(session_id) = decode_prefixed(remote_id.as_str(), SESSION_TABS_REMOTE_ID_PREFIX) {
        let session = store.get_session(&session_id)?;
        return Ok(tab_entries(mount_id, parent_path, &session));
    }
    Ok(Vec::new())
}

fn sessions_root_entry(mount_id: &MountId, parent: &Path) -> TreeEntry {
    directory_entry(
        mount_id,
        RemoteId::new(SESSIONS_ROOT_REMOTE_ID),
        SESSIONS_DIRECTORY_NAME,
        parent.join(SESSIONS_DIRECTORY_NAME),
        None,
    )
}

fn session_directory_entry(
    mount_id: &MountId,
    parent: &Path,
    session: &BrowserSession,
) -> TreeEntry {
    directory_entry(
        mount_id,
        RemoteId::new(session_remote_id(&session.id)),
        session_title(session),
        session_path_with_parent(parent, session),
        Some(remote_version_for_session(session)),
    )
}

fn session_summary_entry(mount_id: &MountId, parent: &Path, session: &BrowserSession) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(session_summary_remote_id(&session.id)),
        kind: EntityKind::Asset,
        title: session_title(session),
        path: parent.join(SESSION_SUMMARY_FILENAME),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(remote_version_for_session(session)),
        stub_frontmatter: None,
    }
}

fn session_tabs_entry(mount_id: &MountId, parent: &Path, session: &BrowserSession) -> TreeEntry {
    directory_entry(
        mount_id,
        RemoteId::new(session_tabs_remote_id(&session.id)),
        TABS_DIRECTORY_NAME,
        parent.join(TABS_DIRECTORY_NAME),
        Some(remote_version_for_session(session)),
    )
}

fn tab_entries(mount_id: &MountId, parent: &Path, session: &BrowserSession) -> Vec<TreeEntry> {
    let mut entries = Vec::new();
    let mut index = 1usize;
    for window in &session.windows {
        for tab in &window.tabs {
            entries.push(tab_entry(mount_id, parent, session, tab, index));
            index += 1;
        }
    }
    entries
}

fn tab_entry(
    mount_id: &MountId,
    parent: &Path,
    session: &BrowserSession,
    tab: &BrowserTab,
    index: usize,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(tab_remote_id(&session.id, &tab.id)),
        kind: EntityKind::Page,
        title: tab_title(tab),
        path: parent
            .join(numbered_directory_name(index, &tab_title(tab)))
            .join(PAGE_FILENAME),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(remote_version_for_tab(session, tab)),
        stub_frontmatter: None,
    }
}

fn directory_entry(
    mount_id: &MountId,
    remote_id: RemoteId,
    title: impl Into<String>,
    path: impl Into<PathBuf>,
    remote_edited_at: Option<String>,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id,
        kind: EntityKind::Directory,
        title: title.into(),
        path: path.into(),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at,
        stub_frontmatter: None,
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

fn session_metadata_json(session: &BrowserSession) -> String {
    let mut value = serde_json::to_value(session).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(object) = &mut value {
        let mut metadata_text = vec![session_title(session), session.id.clone()];
        if let Some(browser) = session.browser.clone() {
            metadata_text.push(browser);
        }
        if let Some(profile) = session.profile.clone() {
            metadata_text.push(profile);
        }
        for tab in session.windows.iter().flat_map(|window| &window.tabs) {
            metadata_text.push(tab.title.clone());
            metadata_text.push(tab.url.clone());
            if let Some(notes) = tab.notes.clone() {
                metadata_text.push(notes);
            }
        }
        let metadata = SearchMetadata {
            metadata_text: dedupe_text(metadata_text),
            aliases: vec![session.id.clone()],
            source_url: None,
        };
        if let Ok(metadata_value) = serde_json::to_value(metadata) {
            object.insert(RAW_SEARCH_METADATA_KEY.to_string(), metadata_value);
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn tab_metadata_json(session: &BrowserSession, tab: &BrowserTab) -> String {
    let mut value = serde_json::to_value(tab).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(object) = &mut value {
        let metadata = SearchMetadata {
            metadata_text: dedupe_text(vec![
                session_title(session),
                session.id.clone(),
                tab.title.clone(),
                tab.url.clone(),
                source_url_host(&tab.url).unwrap_or_default(),
                tab.status.clone().unwrap_or_default(),
                tab.notes.clone().unwrap_or_default(),
                tab.selected_text.clone().unwrap_or_default(),
            ]),
            aliases: vec![tab.id.clone(), tab.url.clone()],
            source_url: Some(tab.url.clone()),
        };
        if let Ok(metadata_value) = serde_json::to_value(metadata) {
            object.insert(RAW_SEARCH_METADATA_KEY.to_string(), metadata_value);
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn dedupe_text(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn session_path(session: &BrowserSession) -> PathBuf {
    PathBuf::from(SESSIONS_DIRECTORY_NAME).join(session_directory_name(session))
}

fn session_path_with_parent(parent: &Path, session: &BrowserSession) -> PathBuf {
    parent.join(session_directory_name(session))
}

fn session_directory_name(session: &BrowserSession) -> String {
    let captured = session
        .captured_at
        .as_deref()
        .and_then(|value| value.get(0..10))
        .unwrap_or("session");
    format!(
        "{}-{}",
        safe_filename(captured, 24),
        safe_filename(&session_title(session), 96)
    )
}

fn numbered_directory_name(number: usize, title: &str) -> String {
    format!("{number:03}-{}", safe_filename(title, 96))
}

fn session_title(session: &BrowserSession) -> String {
    if session.title.trim().is_empty() {
        "Untitled browser session".to_string()
    } else {
        session.title.clone()
    }
}

fn tab_title(tab: &BrowserTab) -> String {
    if tab.title.trim().is_empty() {
        tab.url.clone()
    } else {
        tab.title.clone()
    }
}

fn session_remote_id(session_id: &str) -> String {
    format!("{SESSION_REMOTE_ID_PREFIX}{}", encode_component(session_id))
}

fn session_summary_remote_id(session_id: &str) -> String {
    format!(
        "{SESSION_SUMMARY_REMOTE_ID_PREFIX}{}",
        encode_component(session_id)
    )
}

fn session_tabs_remote_id(session_id: &str) -> String {
    format!(
        "{SESSION_TABS_REMOTE_ID_PREFIX}{}",
        encode_component(session_id)
    )
}

fn tab_remote_id(session_id: &str, tab_id: &str) -> String {
    format!(
        "{TAB_REMOTE_ID_PREFIX}{}:{}",
        encode_component(session_id),
        encode_component(tab_id)
    )
}

fn decode_prefixed(value: &str, prefix: &str) -> Option<String> {
    value.strip_prefix(prefix).and_then(decode_component)
}

fn decode_tab_remote_id(value: &str) -> Option<(String, String)> {
    let remainder = value.strip_prefix(TAB_REMOTE_ID_PREFIX)?;
    let (session_id, tab_id) = remainder.split_once(':')?;
    Some((decode_component(session_id)?, decode_component(tab_id)?))
}

fn encode_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(char::from(*byte));
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

fn decode_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hi = *bytes.get(index + 1)?;
            let lo = *bytes.get(index + 2)?;
            decoded.push((hex_value(hi)? << 4) | hex_value(lo)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn safe_filename(value: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut previous_dash = false;
    for ch in value.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if normalized == '-' {
            if !previous_dash && !out.is_empty() {
                out.push('-');
                previous_dash = true;
            }
        } else {
            out.push(normalized);
            previous_dash = false;
        }
        if out.len() >= max_len {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    }
}

fn source_url_host(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}
