//! Bounded, metadata-only Notion root selection support.
//!
//! This synchronous facade is intentionally separate from connector enumeration:
//! setup needs a small candidate list and exact identity revalidation, never a
//! traversal of block children, database rows, or a selected object's subtree.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::sync::Arc;

use locality_core::model::RemoteId;
use locality_core::{LocalityError, LocalityResult};

use crate::NotionConfig;
use crate::client::{HttpNotionApi, NotionApi};
use crate::dto::{DatabaseDto, PageDto};
use crate::projection::{
    database_title, explicit_root_identity_key, validate_explicit_root_identity,
};
use crate::render::page_title;

/// Maximum number of provider cursor pages read from each Notion search kind.
///
/// One provider-page round may issue one page search and one database search.
/// Exhausted kinds are skipped in later rounds, and all calls stop immediately
/// when the candidate limit is reached.
pub const MAX_ROOT_DISCOVERY_PROVIDER_PAGES: usize = 4;

/// Maximum number of root candidates returned by one discovery call.
pub const MAX_ROOT_DISCOVERY_CANDIDATES: usize = 256;

/// Maximum number of Unicode scalar values retained in a candidate title.
pub const MAX_ROOT_CANDIDATE_TITLE_CHARS: usize = 256;

/// Validated caller-selected bounds for one candidate discovery operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotionRootDiscoveryOptions {
    max_provider_pages: usize,
    max_candidates: usize,
}

impl NotionRootDiscoveryOptions {
    pub fn new(max_provider_pages: usize, max_candidates: usize) -> LocalityResult<Self> {
        if !(1..=MAX_ROOT_DISCOVERY_PROVIDER_PAGES).contains(&max_provider_pages) {
            return Err(LocalityError::InvalidState(format!(
                "Notion root discovery provider-page limit must be between 1 and {MAX_ROOT_DISCOVERY_PROVIDER_PAGES}"
            )));
        }
        if !(1..=MAX_ROOT_DISCOVERY_CANDIDATES).contains(&max_candidates) {
            return Err(LocalityError::InvalidState(format!(
                "Notion root discovery candidate limit must be between 1 and {MAX_ROOT_DISCOVERY_CANDIDATES}"
            )));
        }
        Ok(Self {
            max_provider_pages,
            max_candidates,
        })
    }

    pub fn max_provider_pages(self) -> usize {
        self.max_provider_pages
    }

    pub fn max_candidates(self) -> usize {
        self.max_candidates
    }
}

impl Default for NotionRootDiscoveryOptions {
    fn default() -> Self {
        Self {
            max_provider_pages: MAX_ROOT_DISCOVERY_PROVIDER_PAGES,
            max_candidates: MAX_ROOT_DISCOVERY_CANDIDATES,
        }
    }
}

/// The exact Notion object kind accepted as an explicit root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotionRootCandidateKind {
    Page,
    Database,
}

/// Stable metadata used to present or revalidate one selectable Notion root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotionRootCandidate {
    pub remote_id: RemoteId,
    pub kind: NotionRootCandidateKind,
    pub title: String,
}

/// One bounded discovery result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotionRootDiscovery {
    pub candidates: Vec<NotionRootCandidate>,
    /// True when a provider cursor remained or the requested candidate cap was
    /// reached. Callers must not interpret a truncated list as exhaustive.
    pub truncated: bool,
}

/// Synchronous Notion setup facade for bounded discovery and exact validation.
#[derive(Clone)]
pub struct NotionRootSetup {
    api: Arc<dyn NotionApi>,
}

impl std::fmt::Debug for NotionRootSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotionRootSetup").finish_non_exhaustive()
    }
}

impl NotionRootSetup {
    pub fn new(config: NotionConfig) -> Self {
        Self::with_api(Arc::new(HttpNotionApi::new(config)))
    }

    pub fn with_api(api: Arc<dyn NotionApi>) -> Self {
        Self { api }
    }

    /// Discover a bounded candidate set without traversing any candidate.
    ///
    /// Results are deduplicated by Notion's canonical stable identity. If the
    /// same identity appears as both kinds, the page result wins because page
    /// search is processed first. Returned candidates are ordered by kind
    /// (`Page`, then `Database`) and then canonical stable ID.
    pub fn discover_candidates(
        &self,
        options: NotionRootDiscoveryOptions,
    ) -> LocalityResult<NotionRootDiscovery> {
        let mut candidates = BTreeMap::<String, NotionRootCandidate>::new();
        let mut page_cursor = None;
        let mut database_cursor = None;
        let mut pages_exhausted = false;
        let mut databases_exhausted = false;

        for _ in 0..options.max_provider_pages {
            if candidates.len() == options.max_candidates {
                break;
            }

            if !pages_exhausted {
                let response = self.api.search_pages(page_cursor.as_deref())?;
                insert_pages(&mut candidates, response.results, options.max_candidates);
                let next_cursor =
                    validated_next_cursor("page", response.has_more, response.next_cursor)?;
                pages_exhausted = next_cursor.is_none();
                page_cursor = next_cursor;
                if candidates.len() == options.max_candidates {
                    break;
                }
            }

            if !databases_exhausted {
                let response = self.api.search_databases(database_cursor.as_deref())?;
                insert_databases(&mut candidates, response.results, options.max_candidates);
                let next_cursor =
                    validated_next_cursor("database", response.has_more, response.next_cursor)?;
                databases_exhausted = next_cursor.is_none();
                database_cursor = next_cursor;
            }

            if pages_exhausted && databases_exhausted {
                break;
            }
        }

        let candidate_cap_reached = candidates.len() == options.max_candidates;
        let provider_limit_left_more = !pages_exhausted || !databases_exhausted;
        let mut candidates = candidates.into_values().collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            left.kind.cmp(&right.kind).then_with(|| {
                explicit_root_identity_key(left.remote_id.as_str())
                    .cmp(&explicit_root_identity_key(right.remote_id.as_str()))
            })
        });

        Ok(NotionRootDiscovery {
            candidates,
            truncated: candidate_cap_reached || provider_limit_left_more,
        })
    }

    /// Revalidate one exact stable ID using metadata retrieval only.
    ///
    /// Page lookup always runs first. Database fallback happens only when that
    /// lookup returns `RemoteNotFound`; all other structured errors are returned
    /// unchanged. The provider's returned ID must match the requested identity.
    pub fn validate_root(&self, remote_id: &RemoteId) -> LocalityResult<NotionRootCandidate> {
        match self.api.retrieve_page(remote_id.as_str()) {
            Ok(page) => candidate_from_page(remote_id, page),
            Err(LocalityError::RemoteNotFound(_)) => {
                let database = self.api.retrieve_database(remote_id.as_str())?;
                candidate_from_database(remote_id, database)
            }
            Err(error) => Err(error),
        }
    }
}

fn insert_pages(
    candidates: &mut BTreeMap<String, NotionRootCandidate>,
    pages: Vec<PageDto>,
    max_candidates: usize,
) {
    for page in pages {
        if candidates.len() == max_candidates {
            return;
        }
        let identity = explicit_root_identity_key(&page.id);
        let candidate = NotionRootCandidate {
            remote_id: RemoteId::new(page.id.clone()),
            kind: NotionRootCandidateKind::Page,
            title: bounded_title(page_title(&page)),
        };
        match candidates.entry(identity) {
            Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            Entry::Occupied(mut entry) if entry.get().kind == NotionRootCandidateKind::Database => {
                entry.insert(candidate);
            }
            Entry::Occupied(_) => {}
        }
    }
}

fn insert_databases(
    candidates: &mut BTreeMap<String, NotionRootCandidate>,
    databases: Vec<DatabaseDto>,
    max_candidates: usize,
) {
    for database in databases {
        if candidates.len() == max_candidates {
            return;
        }
        let identity = explicit_root_identity_key(&database.id);
        candidates
            .entry(identity)
            .or_insert_with(|| NotionRootCandidate {
                remote_id: RemoteId::new(database.id.clone()),
                kind: NotionRootCandidateKind::Database,
                title: bounded_title(
                    database_title(&database).unwrap_or_else(|| "Untitled database".to_string()),
                ),
            });
    }
}

fn validated_next_cursor(
    kind: &str,
    has_more: bool,
    next_cursor: Option<String>,
) -> LocalityResult<Option<String>> {
    if has_more && next_cursor.is_none() {
        return Err(LocalityError::InvalidState(format!(
            "notion root discovery {kind} search had has_more without next_cursor"
        )));
    }
    Ok(has_more.then_some(next_cursor).flatten())
}

fn candidate_from_page(requested: &RemoteId, page: PageDto) -> LocalityResult<NotionRootCandidate> {
    validate_explicit_root_identity(requested, &page.id, "page")?;
    Ok(NotionRootCandidate {
        remote_id: RemoteId::new(page.id.clone()),
        kind: NotionRootCandidateKind::Page,
        title: bounded_title(page_title(&page)),
    })
}

fn candidate_from_database(
    requested: &RemoteId,
    database: DatabaseDto,
) -> LocalityResult<NotionRootCandidate> {
    validate_explicit_root_identity(requested, &database.id, "database")?;
    Ok(NotionRootCandidate {
        remote_id: RemoteId::new(database.id.clone()),
        kind: NotionRootCandidateKind::Database,
        title: bounded_title(
            database_title(&database).unwrap_or_else(|| "Untitled database".to_string()),
        ),
    })
}

fn bounded_title(title: String) -> String {
    title.chars().take(MAX_ROOT_CANDIDATE_TITLE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use locality_core::model::RemoteId;
    use locality_core::{LocalityError, LocalityResult};

    use super::{
        MAX_ROOT_CANDIDATE_TITLE_CHARS, MAX_ROOT_DISCOVERY_CANDIDATES,
        MAX_ROOT_DISCOVERY_PROVIDER_PAGES, NotionRootCandidate, NotionRootCandidateKind,
        NotionRootDiscoveryOptions, NotionRootSetup,
    };
    use crate::client::NotionApi;
    use crate::dto::{
        BlockDto, BlockListDto, DataSourceDto, DatabaseDto, DatabaseListDto, PageDto, PageListDto,
        PagePropertyDto, PaginatedListDto, RichTextDto,
    };

    #[test]
    fn options_enforce_public_hard_ceilings() {
        assert!(NotionRootDiscoveryOptions::new(1, 1).is_ok());
        assert!(
            NotionRootDiscoveryOptions::new(
                MAX_ROOT_DISCOVERY_PROVIDER_PAGES,
                MAX_ROOT_DISCOVERY_CANDIDATES,
            )
            .is_ok()
        );
        assert_eq!(
            NotionRootDiscoveryOptions::new(0, 1),
            Err(LocalityError::InvalidState(
                "Notion root discovery provider-page limit must be between 1 and 4".to_string()
            ))
        );
        assert_eq!(
            NotionRootDiscoveryOptions::new(1, MAX_ROOT_DISCOVERY_CANDIDATES + 1),
            Err(LocalityError::InvalidState(
                "Notion root discovery candidate limit must be between 1 and 256".to_string()
            ))
        );
    }

    #[test]
    fn discovery_is_bounded_deduplicated_sorted_and_title_bounded() {
        let long_title = "🦀".repeat(MAX_ROOT_CANDIDATE_TITLE_CHARS + 10);
        let api = Arc::new(FakeNotionApi::default());
        api.push_page_search(page_list(
            vec![
                page("BBBBBBBB-BBBB-BBBB-BBBB-BBBBBBBBBBBB", "Beta"),
                page("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa", &long_title),
            ],
            Some("pages-2"),
        ));
        api.push_database_search(database_list(
            vec![
                database("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "Duplicate"),
                database("cccccccc-cccc-cccc-cccc-cccccccccccc", "Gamma"),
            ],
            None,
        ));
        api.push_page_search(page_list(
            vec![page("DDDDDDDD-DDDD-DDDD-DDDD-DDDDDDDDDDDD", "Delta")],
            Some("pages-3"),
        ));

        let result = NotionRootSetup::with_api(api.clone())
            .discover_candidates(NotionRootDiscoveryOptions::new(2, 4).expect("options"))
            .expect("discovery");

        assert_eq!(
            result.candidates,
            vec![
                NotionRootCandidate {
                    remote_id: RemoteId::new("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
                    kind: NotionRootCandidateKind::Page,
                    title: "🦀".repeat(MAX_ROOT_CANDIDATE_TITLE_CHARS),
                },
                NotionRootCandidate {
                    remote_id: RemoteId::new("BBBBBBBB-BBBB-BBBB-BBBB-BBBBBBBBBBBB"),
                    kind: NotionRootCandidateKind::Page,
                    title: "Beta".to_string(),
                },
                NotionRootCandidate {
                    remote_id: RemoteId::new("DDDDDDDD-DDDD-DDDD-DDDD-DDDDDDDDDDDD"),
                    kind: NotionRootCandidateKind::Page,
                    title: "Delta".to_string(),
                },
                NotionRootCandidate {
                    remote_id: RemoteId::new("cccccccc-cccc-cccc-cccc-cccccccccccc"),
                    kind: NotionRootCandidateKind::Database,
                    title: "Gamma".to_string(),
                },
            ]
        );
        assert!(result.truncated);
        assert_eq!(
            api.calls(),
            vec![
                "search_pages:<first>",
                "search_databases:<first>",
                "search_pages:pages-2",
            ]
        );
    }

    #[test]
    fn discovery_reports_remaining_cursor_at_provider_page_bound() {
        let api = Arc::new(FakeNotionApi::default());
        api.push_page_search(page_list(vec![page("page-1", "One")], Some("pages-2")));
        api.push_database_search(database_list(Vec::new(), None));

        let result = NotionRootSetup::with_api(api.clone())
            .discover_candidates(NotionRootDiscoveryOptions::new(1, 8).expect("options"))
            .expect("discovery");

        assert_eq!(result.candidates.len(), 1);
        assert!(result.truncated);
        assert_eq!(
            api.calls(),
            vec!["search_pages:<first>", "search_databases:<first>"]
        );
    }

    #[test]
    fn discovery_reports_complete_only_after_both_search_kinds_are_exhausted() {
        let api = Arc::new(FakeNotionApi::default());
        api.push_page_search(page_list(vec![page("page-1", "One")], None));
        api.push_database_search(database_list(vec![database("database-1", "Tasks")], None));

        let result = NotionRootSetup::with_api(api.clone())
            .discover_candidates(NotionRootDiscoveryOptions::new(2, 8).expect("options"))
            .expect("discovery");

        assert_eq!(result.candidates.len(), 2);
        assert!(!result.truncated);
        assert_eq!(
            api.calls(),
            vec!["search_pages:<first>", "search_databases:<first>"]
        );
    }

    #[test]
    fn discovery_preserves_structured_search_errors() {
        let error = LocalityError::RateLimited {
            provider: "notion".to_string(),
            retry_after: std::time::Duration::from_secs(3),
            message: "slow down".to_string(),
        };
        let api = Arc::new(FakeNotionApi::default());
        api.page_search_error
            .lock()
            .expect("lock")
            .replace(error.clone());

        assert_eq!(
            NotionRootSetup::with_api(api)
                .discover_candidates(NotionRootDiscoveryOptions::new(1, 8).expect("options")),
            Err(error)
        );
    }

    #[test]
    fn exact_validation_is_page_first_metadata_only_and_accepts_canonical_identity() {
        let api = Arc::new(FakeNotionApi::default());
        api.pages.lock().expect("lock").insert(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            page("AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA", "Roadmap"),
        );

        let candidate = NotionRootSetup::with_api(api.clone())
            .validate_root(&RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
            .expect("valid page");

        assert_eq!(
            candidate,
            NotionRootCandidate {
                remote_id: RemoteId::new("AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA"),
                kind: NotionRootCandidateKind::Page,
                title: "Roadmap".to_string(),
            }
        );
        assert_eq!(
            api.calls(),
            vec!["retrieve_page:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
        );
    }

    #[test]
    fn exact_validation_falls_back_only_on_not_found_and_validates_database_identity() {
        let api = Arc::new(FakeNotionApi::default());
        api.databases
            .lock()
            .expect("lock")
            .insert("database-1".to_string(), database("database-1", "Tasks"));

        assert_eq!(
            NotionRootSetup::with_api(api.clone())
                .validate_root(&RemoteId::new("database-1"))
                .expect("valid database"),
            NotionRootCandidate {
                remote_id: RemoteId::new("database-1"),
                kind: NotionRootCandidateKind::Database,
                title: "Tasks".to_string(),
            }
        );
        assert_eq!(
            api.calls(),
            vec!["retrieve_page:database-1", "retrieve_database:database-1"]
        );

        let api = Arc::new(FakeNotionApi::default());
        api.databases.lock().expect("lock").insert(
            "database-1".to_string(),
            database("different-database", "Tasks"),
        );

        let error = NotionRootSetup::with_api(api.clone())
            .validate_root(&RemoteId::new("database-1"))
            .expect_err("mismatched database identity");

        assert_eq!(
            error,
            LocalityError::InvalidState(
                "Notion explicit root request `database-1` returned database `different-database`"
                    .to_string()
            )
        );
        assert_eq!(
            api.calls(),
            vec!["retrieve_page:database-1", "retrieve_database:database-1"]
        );

        let non_not_found = LocalityError::RateLimited {
            provider: "notion".to_string(),
            retry_after: std::time::Duration::from_secs(2),
            message: "retry".to_string(),
        };
        let api = Arc::new(FakeNotionApi::default());
        api.page_retrieve_error
            .lock()
            .expect("lock")
            .replace(non_not_found.clone());

        assert_eq!(
            NotionRootSetup::with_api(api.clone()).validate_root(&RemoteId::new("database-1")),
            Err(non_not_found)
        );
        assert_eq!(api.calls(), vec!["retrieve_page:database-1"]);
    }

    #[derive(Default)]
    struct FakeNotionApi {
        pages: Mutex<BTreeMap<String, PageDto>>,
        databases: Mutex<BTreeMap<String, DatabaseDto>>,
        page_searches: Mutex<VecDeque<PageListDto>>,
        database_searches: Mutex<VecDeque<DatabaseListDto>>,
        page_search_error: Mutex<Option<LocalityError>>,
        page_retrieve_error: Mutex<Option<LocalityError>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeNotionApi {
        fn push_page_search(&self, page: PageListDto) {
            self.page_searches.lock().expect("lock").push_back(page);
        }

        fn push_database_search(&self, page: DatabaseListDto) {
            self.database_searches.lock().expect("lock").push_back(page);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("lock").clone()
        }
    }

    impl std::fmt::Debug for FakeNotionApi {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FakeNotionApi").finish_non_exhaustive()
        }
    }

    impl NotionApi for FakeNotionApi {
        fn retrieve_page(&self, page_id: &str) -> LocalityResult<PageDto> {
            self.calls
                .lock()
                .expect("lock")
                .push(format!("retrieve_page:{page_id}"));
            if let Some(error) = self.page_retrieve_error.lock().expect("lock").clone() {
                return Err(error);
            }
            self.pages
                .lock()
                .expect("lock")
                .get(page_id)
                .cloned()
                .ok_or_else(|| LocalityError::RemoteNotFound(page_id.to_string()))
        }

        fn retrieve_database(&self, database_id: &str) -> LocalityResult<DatabaseDto> {
            self.calls
                .lock()
                .expect("lock")
                .push(format!("retrieve_database:{database_id}"));
            self.databases
                .lock()
                .expect("lock")
                .get(database_id)
                .cloned()
                .ok_or_else(|| LocalityError::RemoteNotFound(database_id.to_string()))
        }

        fn retrieve_block_children(
            &self,
            _block_id: &str,
            _start_cursor: Option<&str>,
        ) -> LocalityResult<BlockListDto> {
            panic!("root setup must not retrieve block children")
        }

        fn retrieve_data_source(&self, _data_source_id: &str) -> LocalityResult<DataSourceDto> {
            panic!("root setup must not retrieve database data sources")
        }

        fn query_data_source(
            &self,
            _data_source_id: &str,
            _start_cursor: Option<&str>,
        ) -> LocalityResult<PageListDto> {
            panic!("root setup must not query database rows")
        }

        fn search_pages(&self, start_cursor: Option<&str>) -> LocalityResult<PageListDto> {
            self.calls.lock().expect("lock").push(format!(
                "search_pages:{}",
                start_cursor.unwrap_or("<first>")
            ));
            if let Some(error) = self.page_search_error.lock().expect("lock").clone() {
                return Err(error);
            }
            self.page_searches
                .lock()
                .expect("lock")
                .pop_front()
                .ok_or_else(|| LocalityError::InvalidState("unexpected page search".to_string()))
        }

        fn search_databases(&self, start_cursor: Option<&str>) -> LocalityResult<DatabaseListDto> {
            self.calls.lock().expect("lock").push(format!(
                "search_databases:{}",
                start_cursor.unwrap_or("<first>")
            ));
            self.database_searches
                .lock()
                .expect("lock")
                .pop_front()
                .ok_or_else(|| {
                    LocalityError::InvalidState("unexpected database search".to_string())
                })
        }

        fn update_block(
            &self,
            _block_id: &str,
            _body: serde_json::Value,
        ) -> LocalityResult<BlockDto> {
            Err(LocalityError::NotImplemented("update fake block"))
        }

        fn append_block_children(
            &self,
            _block_id: &str,
            _body: serde_json::Value,
        ) -> LocalityResult<BlockListDto> {
            Err(LocalityError::NotImplemented("append fake block children"))
        }

        fn delete_block(&self, _block_id: &str) -> LocalityResult<BlockDto> {
            Err(LocalityError::NotImplemented("delete fake block"))
        }
    }

    fn page_list(results: Vec<PageDto>, next_cursor: Option<&str>) -> PageListDto {
        PaginatedListDto {
            results,
            next_cursor: next_cursor.map(str::to_string),
            has_more: next_cursor.is_some(),
        }
    }

    fn database_list(results: Vec<DatabaseDto>, next_cursor: Option<&str>) -> DatabaseListDto {
        PaginatedListDto {
            results,
            next_cursor: next_cursor.map(str::to_string),
            has_more: next_cursor.is_some(),
        }
    }

    fn page(id: &str, title: &str) -> PageDto {
        PageDto {
            id: id.to_string(),
            parent: None,
            created_time: None,
            last_edited_time: None,
            archived: false,
            in_trash: false,
            properties: BTreeMap::from([(
                "Name".to_string(),
                PagePropertyDto {
                    kind: "title".to_string(),
                    title: vec![rich_text(title)],
                    ..Default::default()
                },
            )]),
        }
    }

    fn database(id: &str, title: &str) -> DatabaseDto {
        DatabaseDto {
            id: id.to_string(),
            title: vec![rich_text(title)],
            ..Default::default()
        }
    }

    fn rich_text(value: &str) -> RichTextDto {
        RichTextDto {
            kind: "text".to_string(),
            plain_text: value.to_string(),
            ..Default::default()
        }
    }
}
