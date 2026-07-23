use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

use crate::client::{ConfluenceApi, HttpConfluenceApiClient};
use crate::dto::{ConfluencePage, ConfluenceSpace};
use crate::render::{
    ConfluenceNativeBundle, remote_version_for_page, remote_version_for_space,
    render_confluence_entity,
};

pub const CONFLUENCE_CONNECTOR_ID: &str = "confluence";

const SPACES_DIRECTORY_NAME: &str = "Spaces";
const PAGES_DIRECTORY_NAME: &str = "Pages";
const SPACE_SUMMARY_FILENAME: &str = "space.md";

const SPACES_ROOT_REMOTE_ID: &str = "confluence:spaces";
const SPACE_REMOTE_ID_PREFIX: &str = "confluence:space:";
const SPACE_SUMMARY_REMOTE_ID_PREFIX: &str = "confluence:space-summary:";
const PAGES_REMOTE_ID_PREFIX: &str = "confluence:pages:";
const PAGE_REMOTE_ID_PREFIX: &str = "confluence:page:";

#[derive(Clone, PartialEq, Eq)]
pub struct ConfluenceConfig {
    pub site_url: String,
    pub email: String,
    pub api_token: String,
    pub execution_policy: ConnectorExecutionPolicy,
}

impl ConfluenceConfig {
    pub fn new(
        site_url: impl Into<String>,
        email: impl Into<String>,
        api_token: impl Into<String>,
    ) -> Self {
        Self {
            site_url: site_url.into(),
            email: email.into(),
            api_token: api_token.into(),
            execution_policy: ConnectorExecutionPolicy::Inline,
        }
    }

    pub fn with_execution_policy(mut self, execution_policy: ConnectorExecutionPolicy) -> Self {
        self.execution_policy = execution_policy;
        self
    }
}

impl fmt::Debug for ConfluenceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfluenceConfig")
            .field("site_url", &self.site_url)
            .field("email", &self.email)
            .field("api_token", &"<redacted>")
            .field("execution_policy", &self.execution_policy)
            .finish()
    }
}

#[derive(Clone)]
pub struct ConfluenceConnector {
    config: ConfluenceConfig,
    api: Arc<dyn ConfluenceApi>,
}

impl fmt::Debug for ConfluenceConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfluenceConnector")
            .field("site_url", &self.config.site_url)
            .field("email", &self.config.email)
            .field("api_token", &"<redacted>")
            .finish()
    }
}

impl ConfluenceConnector {
    pub fn new(config: ConfluenceConfig) -> Self {
        let api = Arc::new(HttpConfluenceApiClient::new(
            config.site_url.clone(),
            config.email.clone(),
            config.api_token.clone(),
        ));
        Self::with_api(config, api)
    }

    pub fn with_api(config: ConfluenceConfig, api: Arc<dyn ConfluenceApi>) -> Self {
        Self { config, api }
    }

    pub fn config(&self) -> &ConfluenceConfig {
        &self.config
    }
}

impl Connector for ConfluenceConnector {
    fn with_execution_policy(&self, policy: ConnectorExecutionPolicy) -> Self {
        Self::new(self.config.clone().with_execution_policy(policy))
    }

    fn kind(&self) -> ConnectorKind {
        ConnectorKind(CONFLUENCE_CONNECTOR_ID)
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities::read_only()
    }

    fn supported_push_operations(&self) -> BTreeSet<PushOperationKind> {
        BTreeSet::new()
    }

    fn enumerate(&self, request: EnumerateRequest) -> LocalityResult<Vec<TreeEntry>> {
        let spaces = self.api.list_spaces()?;
        Ok(entries_for_spaces(&request.mount_id, Path::new(""), spaces))
    }

    fn list_children(&self, request: ListChildrenRequest) -> LocalityResult<ListChildrenResult> {
        let entries = match request.container {
            ChildContainer::Root => {
                vec![spaces_root_entry(&request.mount_id, &request.parent_path)]
            }
            ChildContainer::DirectoryChildren(remote_id)
                if remote_id.as_str() == SPACES_ROOT_REMOTE_ID =>
            {
                self.api
                    .list_spaces()?
                    .into_iter()
                    .map(|space| space_entry(&request.mount_id, &request.parent_path, &space))
                    .collect()
            }
            ChildContainer::DirectoryChildren(remote_id) => list_confluence_directory_children(
                self,
                &request.mount_id,
                &request.parent_path,
                &remote_id,
            )?,
            _ => Vec::new(),
        };
        Ok(ListChildrenResult::complete(entries))
    }

    fn observe(&self, request: ObserveRequest) -> LocalityResult<RemoteObservation> {
        let remote_id = request.remote_id.as_str();
        if let Some(space_id) = remote_id.strip_prefix(SPACE_SUMMARY_REMOTE_ID_PREFIX) {
            let space = self.api.get_space(space_id)?;
            return Ok(observation_from_entry(
                space_summary_entry(&request.mount_id, &space_parent_path(&space), &space),
                Some(RemoteId::new(space_remote_id(&space.id))),
                Some(confluence_space_metadata_json(&space)),
            ));
        }
        if let Some(page_id) = remote_id.strip_prefix(PAGE_REMOTE_ID_PREFIX) {
            let page = self.api.get_page(page_id)?;
            let space = self.api.get_space(&page.space_id)?;
            return Ok(observation_from_entry(
                page_entry(&request.mount_id, &space, &page),
                Some(RemoteId::new(pages_remote_id(&space.id))),
                Some(confluence_page_metadata_json(&space, &page)),
            ));
        }
        Err(LocalityError::Unsupported(
            "Confluence directory observation",
        ))
    }

    fn fetch(&self, request: FetchRequest) -> LocalityResult<NativeEntity> {
        let remote_id = request.remote_id.as_str();
        let bundle = if let Some(space_id) = remote_id.strip_prefix(SPACE_SUMMARY_REMOTE_ID_PREFIX)
        {
            ConfluenceNativeBundle::Space {
                space: self.api.get_space(space_id)?,
            }
        } else if let Some(page_id) = remote_id.strip_prefix(PAGE_REMOTE_ID_PREFIX) {
            let page = self.api.get_page(page_id)?;
            let space = self.api.get_space(&page.space_id)?;
            ConfluenceNativeBundle::Page { space, page }
        } else {
            return Err(LocalityError::Unsupported("Confluence directory hydration"));
        };
        let raw = serde_json::to_vec(&bundle).map_err(|error| {
            LocalityError::Io(format!("Confluence native encode failed: {error}"))
        })?;
        Ok(NativeEntity {
            remote_id: request.remote_id,
            kind: "confluence_entity".to_string(),
            raw,
        })
    }

    fn render(&self, entity: &NativeEntity) -> LocalityResult<CanonicalDocument> {
        let bundle =
            serde_json::from_slice::<ConfluenceNativeBundle>(&entity.raw).map_err(|error| {
                LocalityError::Io(format!("Confluence native decode failed: {error}"))
            })?;
        render_confluence_entity(&bundle)
    }

    fn parse(&self, _document: &CanonicalDocument) -> LocalityResult<ParsedEntity> {
        Err(LocalityError::Unsupported("Confluence writes"))
    }

    fn check_concurrency(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<()> {
        Err(LocalityError::Unsupported("Confluence writes"))
    }

    fn apply(&self, _request: ApplyPlanRequest<'_>) -> LocalityResult<ApplyPlanResult> {
        Err(LocalityError::Unsupported("Confluence writes"))
    }

    fn apply_undo(&self, _request: ApplyUndoRequest<'_>) -> LocalityResult<ApplyUndoResult> {
        Err(LocalityError::Unsupported("Confluence undo"))
    }
}

fn entries_for_spaces(
    mount_id: &MountId,
    parent: &Path,
    spaces: Vec<ConfluenceSpace>,
) -> Vec<TreeEntry> {
    let mut entries = vec![spaces_root_entry(mount_id, parent)];
    for space in spaces {
        let spaces_parent = parent.join(SPACES_DIRECTORY_NAME);
        let space_path = spaces_parent.join(space_directory_name(&space));
        entries.push(space_entry(mount_id, &spaces_parent, &space));
        entries.extend(space_child_entries(mount_id, &space_path, &space));
    }
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.remote_id.cmp(&right.remote_id))
    });
    entries
}

fn list_confluence_directory_children(
    connector: &ConfluenceConnector,
    mount_id: &MountId,
    parent_path: &Path,
    remote_id: &RemoteId,
) -> LocalityResult<Vec<TreeEntry>> {
    if let Some(space_id) = remote_id.as_str().strip_prefix(SPACE_REMOTE_ID_PREFIX) {
        let space = connector.api.get_space(space_id)?;
        return Ok(space_child_entries(mount_id, parent_path, &space));
    }
    if let Some(space_id) = remote_id.as_str().strip_prefix(PAGES_REMOTE_ID_PREFIX) {
        let space = connector.api.get_space(space_id)?;
        return connector.api.list_pages(space_id).map(|pages| {
            pages
                .into_iter()
                .map(|page| page_child_entry(mount_id, parent_path, &space, &page))
                .collect()
        });
    }
    Ok(Vec::new())
}

fn spaces_root_entry(mount_id: &MountId, parent: &Path) -> TreeEntry {
    directory_entry(
        mount_id,
        RemoteId::new(SPACES_ROOT_REMOTE_ID),
        SPACES_DIRECTORY_NAME,
        parent.join(SPACES_DIRECTORY_NAME),
    )
}

fn space_entry(mount_id: &MountId, parent: &Path, space: &ConfluenceSpace) -> TreeEntry {
    directory_entry(
        mount_id,
        RemoteId::new(space_remote_id(&space.id)),
        space.name.clone(),
        parent.join(space_directory_name(space)),
    )
}

fn space_child_entries(
    mount_id: &MountId,
    space_path: &Path,
    space: &ConfluenceSpace,
) -> Vec<TreeEntry> {
    vec![
        space_summary_entry(mount_id, space_path, space),
        directory_entry(
            mount_id,
            RemoteId::new(pages_remote_id(&space.id)),
            PAGES_DIRECTORY_NAME,
            space_path.join(PAGES_DIRECTORY_NAME),
        ),
    ]
}

fn space_summary_entry(
    mount_id: &MountId,
    space_path: &Path,
    space: &ConfluenceSpace,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(space_summary_remote_id(&space.id)),
        kind: EntityKind::Asset,
        title: format!("{} space", space.name),
        path: space_path.join(SPACE_SUMMARY_FILENAME),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(remote_version_for_space(space)),
        stub_frontmatter: None,
    }
}

fn page_entry(mount_id: &MountId, space: &ConfluenceSpace, page: &ConfluencePage) -> TreeEntry {
    page_child_entry(
        mount_id,
        &space_parent_path(space).join(PAGES_DIRECTORY_NAME),
        space,
        page,
    )
}

fn page_child_entry(
    mount_id: &MountId,
    parent: &Path,
    _space: &ConfluenceSpace,
    page: &ConfluencePage,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(page_remote_id(&page.id)),
        kind: EntityKind::Page,
        title: page.title.clone(),
        path: parent.join(page_directory_name(page)).join("page.md"),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(remote_version_for_page(page)),
        stub_frontmatter: None,
    }
}

fn directory_entry(
    mount_id: &MountId,
    remote_id: RemoteId,
    title: impl Into<String>,
    path: impl Into<PathBuf>,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id,
        kind: EntityKind::Directory,
        title: title.into(),
        path: path.into(),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: None,
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

fn confluence_space_metadata_json(space: &ConfluenceSpace) -> String {
    let mut value = serde_json::to_value(space).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(object) = &mut value {
        let metadata = SearchMetadata {
            metadata_text: vec![
                space.id.clone(),
                space.key.clone(),
                space.name.clone(),
                space.status.clone(),
            ],
            aliases: vec![space.key.clone(), space.name.clone()],
            source_url: space.links.webui.clone(),
        };
        if let Ok(metadata_value) = serde_json::to_value(metadata) {
            object.insert(RAW_SEARCH_METADATA_KEY.to_string(), metadata_value);
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn confluence_page_metadata_json(space: &ConfluenceSpace, page: &ConfluencePage) -> String {
    let mut value = serde_json::to_value(page).unwrap_or_else(|_| serde_json::json!({}));
    if let serde_json::Value::Object(object) = &mut value {
        let metadata = SearchMetadata {
            metadata_text: vec![
                space.key.clone(),
                space.name.clone(),
                page.id.clone(),
                page.title.clone(),
                page.status.clone(),
            ],
            aliases: vec![format!("{}:{}", space.key, page.title), page.id.clone()],
            source_url: page.links.webui.clone(),
        };
        if let Ok(metadata_value) = serde_json::to_value(metadata) {
            object.insert(RAW_SEARCH_METADATA_KEY.to_string(), metadata_value);
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

fn space_parent_path(space: &ConfluenceSpace) -> PathBuf {
    PathBuf::from(SPACES_DIRECTORY_NAME).join(space_directory_name(space))
}

fn space_directory_name(space: &ConfluenceSpace) -> String {
    if space.key.trim().is_empty() {
        safe_filename(&space.name, 140)
    } else {
        safe_filename(&format!("{} {}", space.key, space.name), 140)
    }
}

fn page_directory_name(page: &ConfluencePage) -> String {
    let title = safe_filename(&page.title, 120);
    let id = safe_filename(&page.id, 40);
    if title.is_empty() {
        id
    } else {
        format!("{title} {id}")
    }
}

fn safe_filename(value: &str, byte_limit: usize) -> String {
    let mut name = String::new();
    let mut pending_separator = false;
    for character in value.chars() {
        if character.is_control()
            || matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
        {
            pending_separator = true;
            continue;
        }
        if character.is_whitespace() {
            pending_separator = true;
            continue;
        }
        let separator = if pending_separator && !name.is_empty() {
            " "
        } else {
            ""
        };
        if !name.is_empty() && name.len() + separator.len() + character.len_utf8() > byte_limit {
            break;
        }
        name.push_str(separator);
        name.push(character);
        pending_separator = false;
    }
    let name = name.trim_matches([' ', '.', '-']);
    if name.is_empty() {
        "Untitled".to_string()
    } else {
        name.to_string()
    }
}

fn space_remote_id(space_id: &str) -> String {
    format!("{SPACE_REMOTE_ID_PREFIX}{space_id}")
}

fn space_summary_remote_id(space_id: &str) -> String {
    format!("{SPACE_SUMMARY_REMOTE_ID_PREFIX}{space_id}")
}

fn pages_remote_id(space_id: &str) -> String {
    format!("{PAGES_REMOTE_ID_PREFIX}{space_id}")
}

fn page_remote_id(page_id: &str) -> String {
    format!("{PAGE_REMOTE_ID_PREFIX}{page_id}")
}
