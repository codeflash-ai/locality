//! Local metadata search for mounted Locality sources.
//!
//! Search is intentionally store-only. It never calls a connector, so it can be
//! used from the CLI, desktop app, and future agent surfaces without adding
//! network latency to navigation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use locality_core::model::{EntityKind, HydrationState, RemoteId};
use locality_store::{
    ConnectionRecord, ConnectionRepository, EntityRecord, EntityRepository, EntitySearchCandidate,
    EntitySearchDocument, EntitySearchRepository, MountConfig, MountRepository,
    RemoteObservationRecord, RemoteObservationRepository, StoreError,
};
use serde::Serialize;

const DEFAULT_LIMIT: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    pub query: String,
    pub connector: Option<String>,
    pub limit: usize,
    pub include_stale_access: bool,
}

impl SearchOptions {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            connector: None,
            limit: DEFAULT_LIMIT,
            include_stale_access: false,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_context: Option<SearchMatchContext>,
    #[serde(skip_serializing)]
    pub score: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchMatchContext {
    pub field: String,
    pub text: String,
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
    S: MountRepository
        + ConnectionRepository
        + EntityRepository
        + EntitySearchRepository
        + RemoteObservationRepository,
{
    run_search_with_access_roots(store, options, default_access_root)
}

pub fn run_search_with_access_roots<S, F>(
    store: &S,
    options: SearchOptions,
    mount_access_root: F,
) -> Result<SearchReport, SearchError>
where
    S: MountRepository
        + ConnectionRepository
        + EntityRepository
        + EntitySearchRepository
        + RemoteObservationRepository,
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
    let active_connections = active_connections_by_id(store)?;
    let mut matches = Vec::new();

    for mount in mounts
        .into_iter()
        .filter(|mount| connector_matches(mount, options.connector.as_deref()))
        .filter(|mount| {
            options.include_stale_access || mount_has_current_access(mount, &active_connections)
        })
    {
        let access_root = mount_access_root(&mount);

        let mount_matches = if let Some(candidates) = store
            .list_entity_search_candidates(&mount.mount_id, &query, notion_id.as_deref())
            .map_err(SearchError::Store)?
        {
            let mut indexed_matches = search_entity_candidates(
                &mount,
                &access_root,
                &candidates,
                &query,
                notion_id.as_deref(),
            );
            if indexed_matches.len() < options.limit {
                let fallback_matches = search_all_indexed_entities(
                    store,
                    &mount,
                    &access_root,
                    &query,
                    notion_id.as_deref(),
                )?;
                merge_search_results(&mut indexed_matches, fallback_matches);
            }
            indexed_matches
        } else {
            search_all_indexed_entities(store, &mount, &access_root, &query, notion_id.as_deref())?
        };
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
                match_context: None,
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

fn active_connections_by_id<S>(store: &S) -> Result<BTreeMap<String, ConnectionRecord>, SearchError>
where
    S: ConnectionRepository,
{
    Ok(store
        .list_connections()
        .map_err(SearchError::Store)?
        .into_iter()
        .filter(|connection| connection.status == "active")
        .map(|connection| (connection.connection_id.0.clone(), connection))
        .collect())
}

fn mount_has_current_access(
    mount: &MountConfig,
    active_connections: &BTreeMap<String, ConnectionRecord>,
) -> bool {
    let Some(connection_id) = &mount.connection_id else {
        return true;
    };

    active_connections
        .get(&connection_id.0)
        .is_some_and(|connection| connection.connector == mount.connector)
}

fn has_exact_entity_match(results: &[SearchResult], notion_id: Option<&str>) -> bool {
    let Some(notion_id) = notion_id else {
        return false;
    };
    results.iter().any(|result| {
        result.kind != "workspace" && compact_notion_id(&result.remote_id) == notion_id
    })
}

fn merge_search_results(results: &mut Vec<SearchResult>, additional: Vec<SearchResult>) {
    for result in additional {
        if !results.iter().any(|existing| {
            existing.mount_id == result.mount_id && existing.remote_id == result.remote_id
        }) {
            results.push(result);
        }
    }
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
    let url = url.trim();
    let source_host = source_url_host(url);
    if let Some(host) = source_host.as_deref() {
        if !is_notion_url_host(host) {
            return None;
        }

        let without_query = url.split(['?', '#']).next().unwrap_or(url);
        for segment in without_query.rsplit('/') {
            if let Some(candidate) = compact_notion_id_suffix(segment) {
                return Some(candidate);
            }
        }

        return None;
    }

    bare_notion_id(url)
}

pub fn source_url_host(value: &str) -> Option<String> {
    let value = value.trim();
    let (without_scheme, has_scheme) = if let Some((scheme, rest)) = value.split_once("://") {
        if !scheme.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        }) {
            return None;
        }
        (rest, true)
    } else {
        (value, false)
    };
    if !has_scheme && value.chars().any(char::is_whitespace) {
        return None;
    }
    let authority = without_scheme.split(['/', '?', '#']).next()?.trim();
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or(authority)
        .trim()
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if !host.contains('.') {
        return None;
    }
    if !has_scheme && !value.contains('/') && !host.starts_with("www.") {
        return None;
    }
    Some(host)
}

pub fn is_notion_url_host(host: &str) -> bool {
    matches!(host, "notion.so" | "notion.site" | "notion.com")
        || host.ends_with(".notion.so")
        || host.ends_with(".notion.site")
        || host.ends_with(".notion.com")
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
            let matched = indexed_entity_match(entity, observation, None, query, notion_id)?;
            let remote = remote_state(entity, observation);
            let result_path = located_entity_path(entity);
            let state = search_state(&entity.hydration, &remote).to_string();
            Some(SearchResult {
                mount_id: mount.mount_id.0.clone(),
                connector: mount.connector.clone(),
                title: entity.title.clone(),
                kind: entity_kind_name(&entity.kind).to_string(),
                remote_id: entity.remote_id.0.clone(),
                path: locality_platform::logical_path_display(&result_path),
                absolute_path: locality_platform::join_logical_path(access_root, &result_path)
                    .display()
                    .to_string(),
                state,
                safety: search_safety(&entity.hydration, &remote),
                remote,
                match_context: matched.context,
                score: matched.score,
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
                candidate.search_document.as_ref(),
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
    search_document: Option<&EntitySearchDocument>,
    query: &str,
    notion_id: Option<&str>,
) -> Option<SearchResult> {
    let matched = indexed_entity_match(entity, observation, search_document, query, notion_id)?;
    let remote = remote_state(entity, observation);
    let result_path = located_entity_path(entity);
    let state = search_state(&entity.hydration, &remote).to_string();
    Some(SearchResult {
        mount_id: mount.mount_id.0.clone(),
        connector: mount.connector.clone(),
        title: entity.title.clone(),
        kind: entity_kind_name(&entity.kind).to_string(),
        remote_id: entity.remote_id.0.clone(),
        path: locality_platform::logical_path_display(&result_path),
        absolute_path: locality_platform::join_logical_path(access_root, &result_path)
            .display()
            .to_string(),
        state,
        safety: search_safety(&entity.hydration, &remote),
        remote,
        match_context: matched.context,
        score: matched.score,
    })
}

fn indexed_entity_match(
    entity: &EntityRecord,
    observation: Option<&RemoteObservationRecord>,
    search_document: Option<&EntitySearchDocument>,
    query: &str,
    notion_id: Option<&str>,
) -> Option<SearchMatch> {
    if let Some(notion_id) = notion_id {
        if compact_notion_id(&entity.remote_id.0) == notion_id {
            return Some(SearchMatch::new(
                100_000,
                Some(match_context("remote_id", &entity.remote_id.0, "", &[])),
            ));
        }
        if compact_path_id(&entity.path) == notion_id {
            let path = locality_platform::logical_path_display(&entity.path);
            return Some(SearchMatch::new(
                100_000,
                Some(match_context("path", &path, "", &[])),
            ));
        }
        return None;
    }

    let normalized_query = normalize_search_text(query);
    let phrase = normalized_query.trim();
    let tokens = phrase
        .split_whitespace()
        .filter(|token| search_token_allowed(token))
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return None;
    }

    search_fields(entity, observation, search_document)
        .into_iter()
        .filter_map(|field| score_search_field(field, phrase, &tokens))
        .max_by(|left, right| {
            left.score
                .cmp(&right.score)
                .then_with(|| left.context_field().cmp(right.context_field()))
        })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SearchMatch {
    score: i64,
    context: Option<SearchMatchContext>,
}

impl SearchMatch {
    fn new(score: i64, context: Option<SearchMatchContext>) -> Self {
        Self { score, context }
    }

    fn context_field(&self) -> &str {
        self.context
            .as_ref()
            .map(|context| context.field.as_str())
            .unwrap_or("")
    }
}

#[derive(Clone, Copy, Debug)]
struct SearchFieldScores {
    exact: i64,
    prefix: i64,
    contains: i64,
    all_tokens: i64,
    any_tokens: i64,
}

#[derive(Clone, Debug)]
struct SearchField {
    name: &'static str,
    text: String,
    scores: SearchFieldScores,
}

const TITLE_FIELD: SearchFieldScores = SearchFieldScores {
    exact: 90_000,
    prefix: 82_000,
    contains: 74_000,
    all_tokens: 60_000,
    any_tokens: 30_000,
};
const REMOTE_ID_FIELD: SearchFieldScores = SearchFieldScores {
    exact: 91_000,
    prefix: 83_000,
    contains: 75_000,
    all_tokens: 62_000,
    any_tokens: 32_000,
};
const OBSERVED_TITLE_FIELD: SearchFieldScores = SearchFieldScores {
    exact: 88_000,
    prefix: 80_000,
    contains: 72_000,
    all_tokens: 59_000,
    any_tokens: 29_000,
};
const ALIAS_FIELD: SearchFieldScores = SearchFieldScores {
    exact: 89_000,
    prefix: 81_000,
    contains: 73_000,
    all_tokens: 61_000,
    any_tokens: 31_000,
};
const SOURCE_URL_FIELD: SearchFieldScores = SearchFieldScores {
    exact: 87_000,
    prefix: 79_000,
    contains: 71_000,
    all_tokens: 60_000,
    any_tokens: 30_000,
};
const PATH_FIELD: SearchFieldScores = SearchFieldScores {
    exact: 86_000,
    prefix: 78_000,
    contains: 70_000,
    all_tokens: 58_000,
    any_tokens: 28_000,
};
const OBSERVED_PATH_FIELD: SearchFieldScores = SearchFieldScores {
    exact: 84_000,
    prefix: 76_000,
    contains: 68_000,
    all_tokens: 57_000,
    any_tokens: 27_000,
};
const BREADCRUMB_FIELD: SearchFieldScores = SearchFieldScores {
    exact: 76_000,
    prefix: 68_000,
    contains: 62_000,
    all_tokens: 54_000,
    any_tokens: 26_000,
};
const METADATA_FIELD: SearchFieldScores = SearchFieldScores {
    exact: 66_000,
    prefix: 62_000,
    contains: 56_000,
    all_tokens: 42_000,
    any_tokens: 22_000,
};
const FRONTMATTER_FIELD: SearchFieldScores = SearchFieldScores {
    exact: 54_000,
    prefix: 50_000,
    contains: 44_000,
    all_tokens: 24_000,
    any_tokens: 12_000,
};
const BODY_FIELD: SearchFieldScores = SearchFieldScores {
    exact: 28_000,
    prefix: 26_000,
    contains: 25_000,
    all_tokens: 20_000,
    any_tokens: 10_000,
};

fn search_fields(
    entity: &EntityRecord,
    observation: Option<&RemoteObservationRecord>,
    search_document: Option<&EntitySearchDocument>,
) -> Vec<SearchField> {
    let mut fields = Vec::new();
    push_search_field(
        &mut fields,
        "remote_id",
        Some(entity.remote_id.0.as_str()),
        REMOTE_ID_FIELD,
    );
    push_search_field(
        &mut fields,
        "title",
        Some(entity.title.as_str()),
        TITLE_FIELD,
    );
    push_search_field(
        &mut fields,
        "path",
        Some(locality_platform::logical_path_display(&entity.path)),
        PATH_FIELD,
    );
    if let Some(observation) = observation {
        push_search_field(
            &mut fields,
            "remote_title",
            Some(observation.title.as_str()),
            OBSERVED_TITLE_FIELD,
        );
        push_search_field(
            &mut fields,
            "remote_path",
            Some(locality_platform::logical_path_display(
                &observation.projected_path,
            )),
            OBSERVED_PATH_FIELD,
        );
    }
    if let Some(document) = search_document {
        push_search_field(
            &mut fields,
            "alias",
            document.aliases.as_deref(),
            ALIAS_FIELD,
        );
        push_search_field(
            &mut fields,
            "source_url",
            document.source_url.as_deref(),
            SOURCE_URL_FIELD,
        );
        push_search_field(
            &mut fields,
            "metadata",
            document.metadata_text.as_deref(),
            METADATA_FIELD,
        );
        push_search_field(
            &mut fields,
            "breadcrumb",
            document.breadcrumbs.as_deref(),
            BREADCRUMB_FIELD,
        );
        push_search_field(
            &mut fields,
            "frontmatter",
            document.frontmatter.as_deref(),
            FRONTMATTER_FIELD,
        );
        push_search_field(&mut fields, "body", document.body.as_deref(), BODY_FIELD);
    }
    fields
}

fn push_search_field(
    fields: &mut Vec<SearchField>,
    name: &'static str,
    text: Option<impl Into<String>>,
    scores: SearchFieldScores,
) {
    let Some(text) = text else {
        return;
    };
    let text = text.into();
    if !text.trim().is_empty() {
        fields.push(SearchField { name, text, scores });
    }
}

fn score_search_field(field: SearchField, phrase: &str, tokens: &[&str]) -> Option<SearchMatch> {
    let normalized = normalize_search_text(&field.text);
    if normalized.is_empty() {
        return None;
    }
    let score = if normalized == phrase {
        field.scores.exact
    } else if normalized.starts_with(phrase) {
        field.scores.prefix
    } else if normalized.contains(phrase) {
        field.scores.contains
    } else {
        let matched_tokens = tokens
            .iter()
            .filter(|token| normalized.contains(**token))
            .count();
        if matched_tokens == 0 {
            return None;
        }
        if matched_tokens == tokens.len() {
            field.scores.all_tokens + matched_tokens as i64
        } else {
            field.scores.any_tokens + matched_tokens as i64
        }
    };
    Some(SearchMatch::new(
        score,
        Some(match_context(field.name, &field.text, phrase, tokens)),
    ))
}

fn match_context(
    field: impl Into<String>,
    text: &str,
    phrase: &str,
    tokens: &[&str],
) -> SearchMatchContext {
    SearchMatchContext {
        field: field.into(),
        text: snippet_for_match(text, phrase, tokens),
    }
}

fn snippet_for_match(text: &str, phrase: &str, tokens: &[&str]) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lower = trimmed.to_ascii_lowercase();
    let start = (!phrase.is_empty())
        .then(|| lower.find(phrase))
        .flatten()
        .or_else(|| {
            tokens
                .iter()
                .find_map(|token| lower.find(&token.to_ascii_lowercase()))
        })
        .unwrap_or(0);
    bounded_snippet(trimmed, start, 160)
}

fn bounded_snippet(text: &str, match_start: usize, max_len: usize) -> String {
    if text.len() <= max_len {
        return text.split_whitespace().collect::<Vec<_>>().join(" ");
    }

    let half_window = max_len / 2;
    let mut start = match_start.saturating_sub(half_window);
    let mut end = start.saturating_add(max_len).min(text.len());
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    let snippet = text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match (start > 0, end < text.len()) {
        (true, true) => format!("...{snippet}..."),
        (true, false) => format!("...{snippet}"),
        (false, true) => format!("{snippet}..."),
        (false, false) => snippet,
    }
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
            .then(|| locality_platform::logical_path_display(&observation.projected_path)),
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
    mount.root.clone()
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

fn bare_notion_id(value: &str) -> Option<String> {
    if !value
        .chars()
        .all(|character| character.is_ascii_hexdigit() || character == '-')
    {
        return None;
    }

    let compact = compact_notion_id(value);
    (compact.len() == 32).then_some(compact)
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
