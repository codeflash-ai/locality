//! Local metadata search for mounted AgentFS sources.
//!
//! Search is intentionally store-only. It never calls a connector, so it can be
//! used from the CLI, desktop app, and future agent surfaces without adding
//! network latency to navigation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use afs_core::model::{EntityKind, HydrationState, RemoteId};
use afs_store::{
    EntityRecord, EntityRepository, EntitySearchCandidate, EntitySearchRepository, MountConfig,
    MountRepository, ProjectionMode, RemoteObservationRecord, RemoteObservationRepository,
    StoreError,
};
use afsd::virtual_fs::source_root_directory_name;
use serde::Serialize;

const DEFAULT_LIMIT: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    pub query: String,
    pub connector: Option<String>,
    pub limit: usize,
}

impl SearchOptions {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            connector: None,
            limit: DEFAULT_LIMIT,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchReport {
    pub ok: bool,
    pub command: &'static str,
    pub query: String,
    pub connector: Option<String>,
    pub count: usize,
    pub results: Vec<SearchResult>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchResult {
    pub mount_id: String,
    pub connector: String,
    pub title: String,
    pub kind: String,
    pub remote_id: String,
    pub path: String,
    pub absolute_path: String,
    pub state: String,
    pub safety: SearchSafety,
    pub remote: SearchRemoteState,
    #[serde(skip_serializing)]
    pub score: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchSafety {
    pub agent_readable: bool,
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SearchRemoteState {
    pub observed_title: Option<String>,
    pub observed_path: Option<String>,
    pub observed_at: Option<String>,
    pub changed: bool,
    pub deleted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    EmptyQuery,
    InvalidLimit,
    Store(StoreError),
}

impl SearchError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyQuery => "empty_query",
            Self::InvalidLimit => "invalid_limit",
            Self::Store(_) => "store_error",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::EmptyQuery => "search query cannot be empty".to_string(),
            Self::InvalidLimit => "search limit must be greater than zero".to_string(),
            Self::Store(error) => error.to_string(),
        }
    }
}

pub fn run_search<S>(store: &S, options: SearchOptions) -> Result<SearchReport, SearchError>
where
    S: MountRepository + EntityRepository + EntitySearchRepository + RemoteObservationRepository,
{
    run_search_with_access_roots(store, options, default_access_root)
}

pub fn run_search_with_access_roots<S, F>(
    store: &S,
    options: SearchOptions,
    mount_access_root: F,
) -> Result<SearchReport, SearchError>
where
    S: MountRepository + EntityRepository + EntitySearchRepository + RemoteObservationRepository,
    F: Fn(&MountConfig) -> PathBuf,
{
    let query = options.query.trim().to_string();
    if query.is_empty() {
        return Err(SearchError::EmptyQuery);
    }
    if options.limit == 0 {
        return Err(SearchError::InvalidLimit);
    }

    let notion_id = notion_id_from_url(&query);
    let mounts = store.load_mounts().map_err(SearchError::Store)?;
    let mut matches = Vec::new();

    for mount in mounts
        .into_iter()
        .filter(|mount| connector_matches(mount, options.connector.as_deref()))
    {
        let access_root = mount_access_root(&mount);

        let mut mount_matches = if let Some(candidates) = store
            .list_entity_search_candidates(&mount.mount_id, &query, notion_id.as_deref())
            .map_err(SearchError::Store)?
        {
            search_entity_candidates(
                &mount,
                &access_root,
                &candidates,
                &query,
                notion_id.as_deref(),
            )
        } else {
            search_all_indexed_entities(store, &mount, &access_root, &query, notion_id.as_deref())?
        };
        if mount_matches.len() < options.limit {
            mount_matches = search_all_indexed_entities(
                store,
                &mount,
                &access_root,
                &query,
                notion_id.as_deref(),
            )?;
        }
        let has_exact_mount_entity_match =
            has_exact_entity_match(&mount_matches, notion_id.as_deref());
        if mount.remote_root_id.as_ref().is_some_and(|remote_id| {
            notion_id
                .as_ref()
                .is_some_and(|id| compact_notion_id(&remote_id.0) == *id)
        }) && !has_exact_mount_entity_match
        {
            matches.push(SearchResult {
                mount_id: mount.mount_id.0.clone(),
                connector: mount.connector.clone(),
                title: format!("{} root", source_display_name(&mount.connector)),
                kind: "workspace".to_string(),
                remote_id: mount
                    .remote_root_id
                    .as_ref()
                    .map(|remote_id| remote_id.0.clone())
                    .unwrap_or_default(),
                path: ".".to_string(),
                absolute_path: access_root.display().to_string(),
                state: "ready".to_string(),
                safety: SearchSafety {
                    agent_readable: false,
                    labels: vec!["workspace".to_string(), "metadata_only".to_string()],
                },
                remote: SearchRemoteState::default(),
                score: 120_000,
            });
        }

        matches.extend(mount_matches);
    }

    matches.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                normalize_search_text(&left.title).cmp(&normalize_search_text(&right.title))
            })
            .then_with(|| {
                normalize_search_text(&left.path).cmp(&normalize_search_text(&right.path))
            })
            .then_with(|| {
                compact_notion_id(&left.remote_id).cmp(&compact_notion_id(&right.remote_id))
            })
    });

    let results = matches.into_iter().take(options.limit).collect::<Vec<_>>();

    Ok(SearchReport {
        ok: true,
        command: "search",
        query,
        connector: options.connector,
        count: results.len(),
        results,
    })
}

fn has_exact_entity_match(results: &[SearchResult], notion_id: Option<&str>) -> bool {
    let Some(notion_id) = notion_id else {
        return false;
    };
    results.iter().any(|result| {
        result.kind != "workspace" && compact_notion_id(&result.remote_id) == notion_id
    })
}

fn search_all_indexed_entities<S>(
    store: &S,
    mount: &MountConfig,
    access_root: &Path,
    query: &str,
    notion_id: Option<&str>,
) -> Result<Vec<SearchResult>, SearchError>
where
    S: EntityRepository + RemoteObservationRepository,
{
    let observations = store
        .list_remote_observations(&mount.mount_id)
        .map_err(SearchError::Store)?
        .into_iter()
        .map(|observation| (observation.remote_id.clone(), observation))
        .collect::<BTreeMap<_, _>>();
    let entities = store
        .list_entities(&mount.mount_id)
        .map_err(SearchError::Store)?;

    Ok(search_indexed_entities(
        mount,
        access_root,
        &entities,
        &observations,
        query,
        notion_id,
    ))
}

pub fn notion_id_from_url(url: &str) -> Option<String> {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    for segment in without_query.rsplit('/') {
        if let Some(candidate) = compact_notion_id_suffix(segment) {
            return Some(candidate);
        }
    }

    compact_notion_id_suffix(url)
}

pub fn search_indexed_entities(
    mount: &MountConfig,
    access_root: &Path,
    entities: &[EntityRecord],
    observations: &BTreeMap<RemoteId, RemoteObservationRecord>,
    query: &str,
    notion_id: Option<&str>,
) -> Vec<SearchResult> {
    entities
        .iter()
        .filter_map(|entity| {
            let observation = observations.get(&entity.remote_id);
            let score = indexed_entity_score(entity, observation, query, notion_id)?;
            let remote = remote_state(entity, observation);
            let result_path = located_entity_path(entity);
            let state = search_state(&entity.hydration, &remote).to_string();
            Some(SearchResult {
                mount_id: mount.mount_id.0.clone(),
                connector: mount.connector.clone(),
                title: entity.title.clone(),
                kind: entity_kind_name(&entity.kind).to_string(),
                remote_id: entity.remote_id.0.clone(),
                path: afs_platform::logical_path_display(&result_path),
                absolute_path: afs_platform::join_logical_path(access_root, &result_path)
                    .display()
                    .to_string(),
                state,
                safety: search_safety(&entity.hydration, &remote),
                remote,
                score,
            })
        })
        .collect()
}

pub fn search_entity_candidates(
    mount: &MountConfig,
    access_root: &Path,
    candidates: &[EntitySearchCandidate],
    query: &str,
    notion_id: Option<&str>,
) -> Vec<SearchResult> {
    candidates
        .iter()
        .filter_map(|candidate| {
            search_entity_result(
                mount,
                access_root,
                &candidate.entity,
                candidate.observation.as_ref(),
                query,
                notion_id,
            )
        })
        .collect()
}

fn search_entity_result(
    mount: &MountConfig,
    access_root: &Path,
    entity: &EntityRecord,
    observation: Option<&RemoteObservationRecord>,
    query: &str,
    notion_id: Option<&str>,
) -> Option<SearchResult> {
    let score = indexed_entity_score(entity, observation, query, notion_id)?;
    let remote = remote_state(entity, observation);
    let result_path = located_entity_path(entity);
    let state = search_state(&entity.hydration, &remote).to_string();
    Some(SearchResult {
        mount_id: mount.mount_id.0.clone(),
        connector: mount.connector.clone(),
        title: entity.title.clone(),
        kind: entity_kind_name(&entity.kind).to_string(),
        remote_id: entity.remote_id.0.clone(),
        path: afs_platform::logical_path_display(&result_path),
        absolute_path: afs_platform::join_logical_path(access_root, &result_path)
            .display()
            .to_string(),
        state,
        safety: search_safety(&entity.hydration, &remote),
        remote,
        score,
    })
}

fn indexed_entity_score(
    entity: &EntityRecord,
    observation: Option<&RemoteObservationRecord>,
    query: &str,
    notion_id: Option<&str>,
) -> Option<i64> {
    if let Some(notion_id) = notion_id {
        if compact_notion_id(&entity.remote_id.0) == notion_id
            || compact_path_id(&entity.path) == notion_id
        {
            return Some(100_000);
        }
        return None;
    }

    let normalized_query = normalize_search_text(query);
    let phrase = normalized_query.trim();

    let title = normalize_search_text(&entity.title);
    let path = normalize_search_text(&entity.path.to_string_lossy());
    let observed_title = observation
        .map(|observation| normalize_search_text(&observation.title))
        .unwrap_or_default();
    let observed_path = observation
        .map(|observation| normalize_search_text(&observation.projected_path.to_string_lossy()))
        .unwrap_or_default();
    let haystack = format!("{title} {path} {observed_title} {observed_path}");
    let tokens = phrase
        .split_whitespace()
        .filter(|token| search_token_allowed(token))
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return None;
    }

    if title == phrase {
        return Some(90_000);
    }
    if observed_title == phrase {
        return Some(88_000);
    }
    if path == phrase {
        return Some(86_000);
    }
    if observed_path == phrase {
        return Some(84_000);
    }
    if title.starts_with(phrase) {
        return Some(82_000);
    }
    if observed_title.starts_with(phrase) {
        return Some(80_000);
    }
    if path.starts_with(phrase) {
        return Some(78_000);
    }
    if observed_path.starts_with(phrase) {
        return Some(76_000);
    }
    if title.contains(phrase) {
        return Some(74_000);
    }
    if observed_title.contains(phrase) {
        return Some(72_000);
    }
    if path.contains(phrase) {
        return Some(70_000);
    }
    if observed_path.contains(phrase) {
        return Some(68_000);
    }

    let matched_tokens = tokens
        .iter()
        .filter(|token| haystack.contains(**token))
        .count();
    if matched_tokens == 0 {
        return None;
    }

    let all_tokens_matched = matched_tokens == tokens.len();
    let title_bonus = tokens
        .iter()
        .filter(|token| title.contains(**token) || observed_title.contains(**token))
        .count() as i64
        * 500;
    Some(if all_tokens_matched {
        60_000 + title_bonus + matched_tokens as i64
    } else {
        30_000 + title_bonus + matched_tokens as i64
    })
}

fn search_token_allowed(token: &str) -> bool {
    token.len() >= 2 || token.chars().any(|character| character.is_ascii_digit())
}

fn remote_state(
    entity: &EntityRecord,
    observation: Option<&RemoteObservationRecord>,
) -> SearchRemoteState {
    let Some(observation) = observation else {
        return SearchRemoteState::default();
    };
    let title_changed = observation.title != entity.title;
    let path_changed = observation.projected_path != entity.path;
    let version_changed = observation
        .remote_version
        .as_ref()
        .is_some_and(|version| Some(version.0.as_str()) != entity.synced_tree_remote_version());
    let changed = observation.deleted || title_changed || path_changed || version_changed;

    SearchRemoteState {
        observed_title: title_changed.then(|| observation.title.clone()),
        observed_path: path_changed
            .then(|| afs_platform::logical_path_display(&observation.projected_path)),
        observed_at: Some(observation.observed_at.clone()),
        changed,
        deleted: observation.deleted,
    }
}

fn search_state(hydration: &HydrationState, remote: &SearchRemoteState) -> &'static str {
    if remote.deleted {
        return "remote_deleted";
    }
    if matches!(hydration, HydrationState::Dirty) && remote.changed {
        return "review_needed";
    }
    if remote.changed {
        return "remote_update_available";
    }

    match hydration {
        HydrationState::Virtual | HydrationState::Stub => "online_only",
        HydrationState::Hydrated => "ready",
        HydrationState::Dirty => "pending_changes",
        HydrationState::Conflicted => "conflict",
    }
}

fn search_safety(hydration: &HydrationState, remote: &SearchRemoteState) -> SearchSafety {
    let state = search_state(hydration, remote);
    let labels = match state {
        "ready" => vec!["ready"],
        "online_only" => vec!["online_only", "metadata_only"],
        "pending_changes" => vec!["local_changes", "needs_review"],
        "conflict" => vec!["conflict", "needs_review"],
        "remote_update_available" => vec!["remote_changed", "stale_local"],
        "remote_deleted" => vec!["remote_deleted", "do_not_read"],
        "review_needed" => vec!["local_and_remote_changed", "needs_review"],
        _ => vec!["unknown"],
    }
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();

    SearchSafety {
        agent_readable: state == "ready",
        labels,
    }
}

fn located_entity_path(entity: &EntityRecord) -> PathBuf {
    if entity.kind == EntityKind::Page
        && entity
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("md")
    {
        return entity.path.with_extension("md");
    }

    entity.path.clone()
}

fn connector_matches(mount: &MountConfig, connector: Option<&str>) -> bool {
    connector.is_none_or(|connector| mount.connector == connector)
}

fn source_display_name(connector: &str) -> String {
    match connector {
        "notion" => "Notion".to_string(),
        other => other.to_string(),
    }
}

fn default_access_root(mount: &MountConfig) -> PathBuf {
    match mount.projection {
        ProjectionMode::LinuxFuse | ProjectionMode::WindowsCloudFiles => mount
            .root
            .join(source_root_directory_name(&mount.connector)),
        ProjectionMode::PlainFiles | ProjectionMode::MacosFileProvider => mount.root.clone(),
    }
}

fn entity_kind_name(kind: &EntityKind) -> &str {
    match kind {
        EntityKind::Page => "page",
        EntityKind::Database => "database",
        EntityKind::Directory => "directory",
        EntityKind::Asset => "asset",
        EntityKind::Unknown(_) => "unknown",
    }
}

fn compact_path_id(path: &Path) -> String {
    compact_notion_id_suffix(&path.to_string_lossy()).unwrap_or_default()
}

fn compact_notion_id_suffix(value: &str) -> Option<String> {
    let compact = compact_notion_id(value);
    if compact.len() < 32 {
        return None;
    }

    Some(compact[compact.len() - 32..].to_string())
}

fn compact_notion_id(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>()
        .to_lowercase()
}

fn normalize_search_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
