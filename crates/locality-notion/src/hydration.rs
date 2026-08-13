//! Opt-in bounded primitives for one Notion initial-hydration job.
//!
//! These functions do not create or retain a session. A later host workflow
//! must construct exactly one [`InitialHydrationBudget`] and pass it through
//! discovery, fetch, media, render, and projection.

use std::collections::BTreeSet;
use std::io::Write;

use locality_connector::hydration_budget::{
    HydrationResource, InitialHydrationBudget, InitialHydrationError, InitialHydrationResult,
};
use locality_connector::{NativeEntity, PortableRenderRequest, PortableRenderResult};
use locality_core::canonical::render_canonical_markdown;
use locality_core::model::RemoteId;
use serde::Serialize;

use crate::client::NotionApi;
use crate::dto::{BlockTreeDto, NotionDatabaseBundle, NotionPageBundle, PageDto};
use crate::fetch::should_fetch_children_for_bounded_hydration;
use crate::media::{
    PortableMediaCapture, PortableMediaCaptureFetcher, validate_portable_hosted_media_url,
};
use crate::render::{NotionRenderedEntity, render_native_entity};

/// Fetch and encode one page using only budget-aware provider calls.
pub fn fetch_page_native_bounded(
    api: &dyn NotionApi,
    page_id: &str,
    budget: &InitialHydrationBudget,
) -> InitialHydrationResult<NativeEntity> {
    let mut temporary = TemporaryRetainedBytes::new(budget);
    budget.visit_traversal_node(0)?;
    let page = api.retrieve_page_bounded(page_id, budget)?;
    validate_requested_identity(page_id, &page.id)?;
    temporary.reserve(encoded_len(&page)?)?;
    let blocks = fetch_block_trees_bounded(api, page_id, 1, budget, &mut temporary)?;
    let remote_id = RemoteId::new(page.id.clone());
    let kind = "notion_page".to_string();
    budget.account_retained_bytes(remote_id.as_str().len() + kind.len())?;
    let bundle = NotionPageBundle { page, blocks };
    temporary.replace(encoded_len(&bundle)?)?;
    let raw = encode_native_json_bounded(&bundle, budget)?;
    Ok(NativeEntity {
        remote_id,
        kind,
        raw,
    })
}

/// Fetch and encode one database container and its declared data-source
/// schemas. Row enumeration remains a separate bounded projection step.
pub fn fetch_database_native_bounded(
    api: &dyn NotionApi,
    database_id: &str,
    budget: &InitialHydrationBudget,
) -> InitialHydrationResult<NativeEntity> {
    let mut temporary = TemporaryRetainedBytes::new(budget);
    budget.visit_traversal_node(0)?;
    let database = api.retrieve_database_bounded(database_id, budget)?;
    validate_requested_identity(database_id, &database.id)?;
    let inventory_bytes = encoded_len(&database.data_sources)?;
    budget.account_inventory(database.data_sources.len(), inventory_bytes)?;
    temporary.adopt(inventory_bytes)?;
    temporary.replace(encoded_len(&database)?)?;
    let mut seen = BTreeSet::new();
    let mut data_sources = Vec::new();
    data_sources
        .try_reserve(database.data_sources.len())
        .map_err(|_| InitialHydrationError::ProviderUnavailable)?;
    for summary in &database.data_sources {
        let canonical = canonical_notion_id(&summary.id);
        if canonical.is_empty() || !seen.insert(canonical) {
            return Err(InitialHydrationError::ProviderResponseInvalid);
        }
        budget.preflight_traversal_node(1)?;
        let data_source = api.retrieve_data_source_bounded(&summary.id, budget)?;
        validate_requested_identity(&summary.id, &data_source.id)?;
        let parent_database_id = data_source
            .parent
            .as_ref()
            .and_then(|parent| parent.database_id.as_deref())
            .ok_or(InitialHydrationError::ProviderResponseInvalid)?;
        validate_requested_identity(&database.id, parent_database_id)?;
        budget.visit_traversal_node(1)?;
        temporary.reserve(encoded_len(&data_source)?)?;
        data_sources.push(data_source);
    }
    let remote_id = RemoteId::new(database.id.clone());
    let kind = "notion_database".to_string();
    budget.account_retained_bytes(remote_id.as_str().len() + kind.len())?;
    let bundle = NotionDatabaseBundle {
        database,
        data_sources,
    };
    temporary.replace(encoded_len(&bundle)?)?;
    let raw = encode_native_json_bounded(&bundle, budget)?;
    Ok(NativeEntity {
        remote_id,
        kind,
        raw,
    })
}

/// Retrieve a data source's row inventory without assigning snapshot or
/// deletion authority. Every page, row, cursor, and retained byte shares the
/// caller's job budget.
pub fn query_data_source_rows_bounded(
    api: &dyn NotionApi,
    data_source_id: &str,
    budget: &InitialHydrationBudget,
) -> InitialHydrationResult<Vec<PageDto>> {
    budget.visit_traversal_node(0)?;
    let mut retained_rows = TemporaryRetainedBytes::new(budget);
    let mut retained_cursors = TemporaryRetainedBytes::new(budget);
    let mut cursors = Vec::new();
    let mut seen_rows = BTreeSet::new();
    let mut rows = Vec::new();
    loop {
        budget.preflight_traversal_node(1)?;
        let page = api.query_data_source_bounded(
            data_source_id,
            cursors.last().map(String::as_str),
            budget,
        )?;
        let encoded_bytes = encoded_len(&page.results)?;
        budget.account_inventory(page.results.len(), encoded_bytes)?;
        retained_rows.adopt(encoded_bytes)?;
        rows.try_reserve(page.results.len())
            .map_err(|_| InitialHydrationError::ProviderUnavailable)?;
        for row in page.results {
            let identity = canonical_notion_id(&row.id);
            if identity.is_empty() || !seen_rows.insert(identity) {
                return Err(InitialHydrationError::ProviderResponseInvalid);
            }
            let parent_data_source_id = row
                .parent
                .as_ref()
                .and_then(|parent| parent.data_source_id.as_deref())
                .ok_or(InitialHydrationError::ProviderResponseInvalid)?;
            validate_requested_identity(data_source_id, parent_data_source_id)?;
            budget.visit_traversal_node(1)?;
            rows.push(row);
        }
        if !page.has_more {
            break;
        }
        let Some(next_cursor) = page.next_cursor.filter(|cursor| !cursor.is_empty()) else {
            return Err(InitialHydrationError::ProviderResponseInvalid);
        };
        if cursors.iter().any(|cursor| cursor == &next_cursor) {
            return Err(InitialHydrationError::ProviderResponseInvalid);
        }
        retained_cursors.reserve(next_cursor.len())?;
        cursors.push(next_cursor);
    }
    retained_rows.replace_and_commit(encoded_len(&rows)?)?;
    Ok(rows)
}

fn fetch_block_trees_bounded(
    api: &dyn NotionApi,
    block_id: &str,
    depth: usize,
    budget: &InitialHydrationBudget,
    temporary: &mut TemporaryRetainedBytes<'_>,
) -> InitialHydrationResult<Vec<BlockTreeDto>> {
    let mut retained_cursors = TemporaryRetainedBytes::new(budget);
    let mut cursors = Vec::new();
    let mut trees = Vec::new();
    loop {
        budget.preflight_traversal_node(depth)?;
        let page = api.retrieve_block_children_bounded(
            block_id,
            cursors.last().map(String::as_str),
            budget,
        )?;
        let encoded_bytes = encoded_len(&page.results)?;
        budget.account_inventory(page.results.len(), encoded_bytes)?;
        temporary.adopt(encoded_bytes)?;
        trees
            .try_reserve(page.results.len())
            .map_err(|_| InitialHydrationError::ProviderUnavailable)?;
        for block in page.results {
            budget.visit_traversal_node(depth)?;
            let children =
                if should_fetch_children_for_bounded_hydration(&block.kind, block.has_children) {
                    let child_depth =
                        depth
                            .checked_add(1)
                            .ok_or(InitialHydrationError::LimitExceeded {
                                resource: HydrationResource::TraversalDepth,
                            })?;
                    fetch_block_trees_bounded(api, &block.id, child_depth, budget, temporary)?
                } else {
                    Vec::new()
                };
            trees.push(BlockTreeDto { block, children });
        }
        if !page.has_more {
            break;
        }
        let Some(next_cursor) = page.next_cursor.filter(|cursor| !cursor.is_empty()) else {
            return Err(InitialHydrationError::ProviderResponseInvalid);
        };
        if cursors.iter().any(|cursor| cursor == &next_cursor) {
            return Err(InitialHydrationError::ProviderResponseInvalid);
        }
        retained_cursors.reserve(next_cursor.len())?;
        cursors.push(next_cursor);
    }
    Ok(trees)
}

/// Serialize connector-native JSON into a budgeted buffer. Serde writes are
/// charged before the destination grows, including base64 expansion of media.
pub fn encode_native_json_bounded<T: Serialize>(
    value: &T,
    budget: &InitialHydrationBudget,
) -> InitialHydrationResult<Vec<u8>> {
    let mut writer = BudgetedNativeWriter {
        budget,
        bytes: Vec::new(),
        error: None,
    };
    if serde_json::to_writer(&mut writer, value).is_err() {
        return Err(writer
            .error
            .unwrap_or(InitialHydrationError::ProviderResponseInvalid));
    }
    Ok(writer.bytes)
}

struct BudgetedNativeWriter<'a> {
    budget: &'a InitialHydrationBudget,
    bytes: Vec<u8>,
    error: Option<InitialHydrationError>,
}

impl Write for BudgetedNativeWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Err(error) = self.budget.account_native_bytes(bytes.len()) {
            self.error = Some(error);
            return Err(std::io::Error::other("initial hydration native limit"));
        }
        if self.bytes.try_reserve(bytes.len()).is_err() {
            self.error = Some(InitialHydrationError::ProviderUnavailable);
            return Err(std::io::Error::other("initial hydration allocation"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Fetch one hosted asset after reserving its slot. The shared remaining byte
/// allowance is passed to the fetcher, then the actual decoded bytes are
/// charged before the returned vector can be retained by the caller.
pub fn fetch_media_bounded(
    fetcher: &dyn PortableMediaCaptureFetcher,
    hosted_url: &str,
    per_asset_max_bytes: usize,
    budget: &InitialHydrationBudget,
) -> InitialHydrationResult<PortableMediaCapture> {
    validate_portable_hosted_media_url(hosted_url)
        .map_err(InitialHydrationError::from_connector_error)?;
    let remaining_response = budget.remaining(HydrationResource::ResponseBodyBytes)?;
    if remaining_response == 0 {
        return Err(InitialHydrationError::LimitExceeded {
            resource: HydrationResource::ResponseBodyBytes,
        });
    }
    let requested = usize::try_from(remaining_response)
        .unwrap_or(usize::MAX)
        .min(per_asset_max_bytes);
    let media_reservation = budget.reserve_media_bytes(requested)?;
    budget.reserve_media_fetch()?;
    let maximum = media_reservation.maximum_bytes();
    let capture = fetcher.fetch_bounded(hosted_url, maximum, budget);
    budget.check_deadline()?;
    let capture = capture?;
    if capture.bytes.len() > maximum {
        return Err(InitialHydrationError::LimitExceeded {
            resource: HydrationResource::MediaDecodedBytes,
        });
    }
    media_reservation.commit(capture.bytes.len(), capture.media_type.len())?;
    Ok(capture)
}

/// Render a page native entity and charge its canonical output and one page
/// projection. Existing unbounded rendering remains unchanged.
pub fn render_native_entity_bounded(
    entity: &NativeEntity,
    budget: &InitialHydrationBudget,
) -> InitialHydrationResult<NotionRenderedEntity> {
    budget.validate_native_input_bytes(entity.raw.len())?;
    budget.preflight_rendered_content(1)?;
    budget.preflight_projections(1, 0)?;
    let rendered =
        render_native_entity(entity).map_err(InitialHydrationError::from_connector_error)?;
    let canonical_bytes = render_canonical_markdown(&rendered.document).len();
    let retained_bytes = encoded_len(&rendered)?;
    budget.account_render_output(canonical_bytes, 1, retained_bytes)?;
    Ok(rendered)
}

/// Render the existing portable formats under the same output accounting.
pub fn render_portable_bounded(
    request: &PortableRenderRequest,
    budget: &InitialHydrationBudget,
) -> InitialHydrationResult<PortableRenderResult> {
    budget.validate_native_input_bytes(request.native.raw.len())?;
    budget.preflight_rendered_content(1)?;
    budget.preflight_projections(1, 0)?;
    let rendered =
        crate::portable::render(request).map_err(InitialHydrationError::from_connector_error)?;
    let content_bytes = rendered
        .projections
        .iter()
        .try_fold(rendered.canonical.body.len(), |total, projection| {
            total.checked_add(projection.artifact.body.len())
        })
        .ok_or(InitialHydrationError::LimitExceeded {
            resource: HydrationResource::RenderedContentBytes,
        })?;
    let retained_bytes = encoded_len(&rendered)?;
    budget.account_render_output(content_bytes, rendered.projections.len(), retained_bytes)?;
    Ok(rendered)
}

/// Charge a connector change page before it is appended to an aggregate.
pub fn account_changes_bounded(
    change_count: usize,
    retained_bytes: usize,
    budget: &InitialHydrationBudget,
) -> InitialHydrationResult<()> {
    budget.account_changes(change_count, retained_bytes)
}

fn encoded_len<T: Serialize>(value: &T) -> InitialHydrationResult<usize> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("encoded length overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)
        .map_err(|_| InitialHydrationError::ProviderResponseInvalid)?;
    Ok(counter.0)
}

struct TemporaryRetainedBytes<'a> {
    budget: &'a InitialHydrationBudget,
    bytes: usize,
}

impl<'a> TemporaryRetainedBytes<'a> {
    fn new(budget: &'a InitialHydrationBudget) -> Self {
        Self { budget, bytes: 0 }
    }

    fn adopt(&mut self, bytes: usize) -> InitialHydrationResult<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RetainedBytes,
            })?;
        Ok(())
    }

    fn reserve(&mut self, bytes: usize) -> InitialHydrationResult<()> {
        self.budget.account_retained_bytes(bytes)?;
        self.adopt(bytes)
    }

    fn replace(&mut self, bytes: usize) -> InitialHydrationResult<()> {
        self.budget.replace_retained_bytes(self.bytes, bytes)?;
        self.bytes = bytes;
        Ok(())
    }

    fn replace_and_commit(&mut self, bytes: usize) -> InitialHydrationResult<()> {
        self.replace(bytes)?;
        self.bytes = 0;
        Ok(())
    }
}

impl Drop for TemporaryRetainedBytes<'_> {
    fn drop(&mut self) {
        if self.bytes > 0 {
            let _ = self.budget.release_retained_bytes(self.bytes);
        }
    }
}

fn validate_requested_identity(requested: &str, returned: &str) -> InitialHydrationResult<()> {
    if canonical_notion_id(requested).is_empty()
        || canonical_notion_id(requested) != canonical_notion_id(returned)
    {
        return Err(InitialHydrationError::ProviderResponseInvalid);
    }
    Ok(())
}

fn canonical_notion_id(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};

    use locality_connector::hydration_budget::InitialHydrationLimits;
    use locality_core::portable::{LogicalPath, SourceConnectionId};
    use locality_core::{LocalityError, LocalityResult};

    use super::*;
    use crate::dto::{
        BlockDto, BlockListDto, NotionPortableCapturedMediaV1, PageDto, PageListDto,
        PaginatedListDto,
    };

    fn limits(cap: u64) -> InitialHydrationLimits {
        InitialHydrationLimits {
            max_response_body_bytes: cap,
            max_provider_calls: cap,
            provider_deadline_ms: 60_000,
            max_inventory_items: cap,
            max_inventory_encoded_bytes: cap,
            max_traversal_nodes: cap,
            max_traversal_depth: cap,
            max_native_bytes: cap,
            max_media_assets: cap,
            max_media_decoded_bytes: cap,
            max_rendered_content_bytes: cap,
            max_projections: cap,
            max_changes: cap,
            max_retained_bytes: cap,
        }
    }

    #[derive(Debug)]
    struct TreeApi {
        calls: Arc<AtomicUsize>,
        nested: bool,
    }

    impl NotionApi for TreeApi {
        fn retrieve_page(&self, page_id: &str) -> LocalityResult<PageDto> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(PageDto {
                id: page_id.to_string(),
                parent: None,
                created_time: None,
                last_edited_time: None,
                archived: false,
                in_trash: false,
                properties: Default::default(),
            })
        }

        fn retrieve_block_children(
            &self,
            block_id: &str,
            _start_cursor: Option<&str>,
        ) -> LocalityResult<BlockListDto> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let results = match block_id {
                "page" => vec![BlockDto {
                    id: "child".to_string(),
                    kind: "toggle".to_string(),
                    has_children: self.nested,
                    ..Default::default()
                }],
                "child" => vec![BlockDto {
                    id: "grandchild".to_string(),
                    kind: "paragraph".to_string(),
                    ..Default::default()
                }],
                _ => Vec::new(),
            };
            Ok(PaginatedListDto {
                results,
                next_cursor: None,
                has_more: false,
            })
        }

        fn query_data_source(
            &self,
            _data_source_id: &str,
            start_cursor: Option<&str>,
        ) -> LocalityResult<PageListDto> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if start_cursor.is_some() {
                return Err(LocalityError::Io("terminal scripted call".to_string()));
            }
            Ok(PaginatedListDto {
                results: Vec::new(),
                next_cursor: Some("abcd".to_string()),
                has_more: true,
            })
        }

        fn search_pages(
            &self,
            _start_cursor: Option<&str>,
        ) -> LocalityResult<crate::dto::PageListDto> {
            Err(LocalityError::NotImplemented("test"))
        }

        fn update_block(
            &self,
            _block_id: &str,
            _body: serde_json::Value,
        ) -> LocalityResult<BlockDto> {
            Err(LocalityError::NotImplemented("test"))
        }

        fn append_block_children(
            &self,
            _block_id: &str,
            _body: serde_json::Value,
        ) -> LocalityResult<BlockListDto> {
            Err(LocalityError::NotImplemented("test"))
        }

        fn delete_block(&self, _block_id: &str) -> LocalityResult<BlockDto> {
            Err(LocalityError::NotImplemented("test"))
        }

        fn retrieve_page_bounded(
            &self,
            page_id: &str,
            budget: &InitialHydrationBudget,
        ) -> InitialHydrationResult<PageDto> {
            bounded_test_call(budget, || self.retrieve_page(page_id))
        }

        fn retrieve_block_children_bounded(
            &self,
            block_id: &str,
            start_cursor: Option<&str>,
            budget: &InitialHydrationBudget,
        ) -> InitialHydrationResult<BlockListDto> {
            bounded_test_call(budget, || {
                self.retrieve_block_children(block_id, start_cursor)
            })
        }

        fn query_data_source_bounded(
            &self,
            data_source_id: &str,
            start_cursor: Option<&str>,
            budget: &InitialHydrationBudget,
        ) -> InitialHydrationResult<PageListDto> {
            bounded_test_call(budget, || {
                self.query_data_source(data_source_id, start_cursor)
            })
        }
    }

    fn bounded_test_call<T: Serialize>(
        budget: &InitialHydrationBudget,
        call: impl FnOnce() -> LocalityResult<T>,
    ) -> InitialHydrationResult<T> {
        budget.reserve_provider_call()?;
        let value = call().map_err(InitialHydrationError::from_connector_error)?;
        let encoded = serde_json::to_vec(&value)
            .map_err(|_| InitialHydrationError::ProviderResponseInvalid)?;
        budget.account_response_chunk(encoded.len())?;
        Ok(value)
    }

    #[test]
    fn provider_budget_stops_before_the_next_api_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let api = TreeApi {
            calls: Arc::clone(&calls),
            nested: false,
        };
        let mut configured = limits(1_000_000);
        configured.max_provider_calls = 1;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        let error = fetch_page_native_bounded(&api, "page", &budget).unwrap_err();
        assert_eq!(
            error,
            InitialHydrationError::LimitExceeded {
                resource: HydrationResource::ProviderCalls
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn depth_is_rejected_before_fetching_the_next_child_page() {
        let calls = Arc::new(AtomicUsize::new(0));
        let api = TreeApi {
            calls: Arc::clone(&calls),
            nested: true,
        };
        let mut configured = limits(1_000_000);
        configured.max_traversal_depth = 1;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        let error = fetch_page_native_bounded(&api, "page", &budget).unwrap_err();
        assert_eq!(
            error,
            InitialHydrationError::LimitExceeded {
                resource: HydrationResource::TraversalDepth
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn excessive_depth_rejects_before_the_first_child_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let api = TreeApi {
            calls: Arc::clone(&calls),
            nested: false,
        };
        let mut configured = limits(1_000_000);
        configured.max_traversal_depth = 1;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        let mut temporary = TemporaryRetainedBytes::new(&budget);
        let error =
            fetch_block_trees_bounded(&api, "page", 2, &budget, &mut temporary).unwrap_err();
        assert_eq!(
            error,
            InitialHydrationError::LimitExceeded {
                resource: HydrationResource::TraversalDepth
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_result_releases_inventory_and_retains_complete_identity() {
        let calls = Arc::new(AtomicUsize::new(0));
        let api = TreeApi {
            calls,
            nested: false,
        };
        let configured = limits(1_000_000);
        let budget = InitialHydrationBudget::new(configured).unwrap();
        let entity = fetch_page_native_bounded(&api, "page", &budget).unwrap();
        let retained = configured.max_retained_bytes
            - budget.remaining(HydrationResource::RetainedBytes).unwrap();
        assert_eq!(
            retained,
            (entity.raw.len() + entity.remote_id.as_str().len() + entity.kind.len()) as u64
        );
    }

    #[test]
    fn cursor_is_retained_before_cycle_storage_at_cap_and_cap_plus_one() {
        for (cap, expected_calls, expected_error) in [
            (6, 2, InitialHydrationError::ProviderUnavailable),
            (
                5,
                1,
                InitialHydrationError::LimitExceeded {
                    resource: HydrationResource::RetainedBytes,
                },
            ),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let api = TreeApi {
                calls: Arc::clone(&calls),
                nested: false,
            };
            let mut configured = limits(1_000_000);
            configured.max_retained_bytes = cap;
            let budget = InitialHydrationBudget::new(configured).unwrap();
            assert_eq!(
                query_data_source_rows_bounded(&api, "source", &budget),
                Err(expected_error)
            );
            assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
        }
    }

    #[test]
    fn native_limit_counts_base64_expansion_at_cap_and_cap_plus_one() {
        let value = NotionPortableCapturedMediaV1 {
            block_id: "b".to_string(),
            kind: "file".to_string(),
            media_type: "application/octet-stream".to_string(),
            bytes: vec![1, 2, 3, 4],
        };
        let exact = serde_json::to_vec(&value).unwrap().len() as u64;
        assert!(exact > value.bytes.len() as u64);

        let mut exact_limits = limits(exact.max(100));
        exact_limits.max_native_bytes = exact;
        exact_limits.max_retained_bytes = exact;
        let exact_budget = InitialHydrationBudget::new(exact_limits).unwrap();
        assert_eq!(
            encode_native_json_bounded(&value, &exact_budget)
                .unwrap()
                .len() as u64,
            exact
        );

        let mut short_limits = exact_limits;
        short_limits.max_native_bytes = exact - 1;
        let short_budget = InitialHydrationBudget::new(short_limits).unwrap();
        assert_eq!(
            encode_native_json_bounded(&value, &short_budget).unwrap_err(),
            InitialHydrationError::LimitExceeded {
                resource: HydrationResource::NativeBytes
            }
        );
    }

    #[derive(Debug)]
    struct CountingFetcher {
        calls: Arc<AtomicUsize>,
        bytes: Vec<u8>,
    }

    impl PortableMediaCaptureFetcher for CountingFetcher {
        fn fetch(
            &self,
            _hosted_url: &str,
            max_bytes: usize,
        ) -> LocalityResult<PortableMediaCapture> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.bytes.len() > max_bytes {
                return Err(LocalityError::InvalidState("too large".to_string()));
            }
            Ok(PortableMediaCapture {
                bytes: self.bytes.clone(),
                media_type: "application/octet-stream".to_string(),
            })
        }

        fn fetch_bounded(
            &self,
            _hosted_url: &str,
            max_bytes: usize,
            budget: &InitialHydrationBudget,
        ) -> InitialHydrationResult<PortableMediaCapture> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            budget.account_response_chunk(self.bytes.len())?;
            if self.bytes.len() > max_bytes {
                return Err(InitialHydrationError::LimitExceeded {
                    resource: HydrationResource::MediaDecodedBytes,
                });
            }
            Ok(PortableMediaCapture {
                bytes: self.bytes.clone(),
                media_type: "application/octet-stream".to_string(),
            })
        }
    }

    #[test]
    fn media_aggregate_exhaustion_prevents_the_next_fetch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let fetcher = CountingFetcher {
            calls: Arc::clone(&calls),
            bytes: vec![1, 2, 3],
        };
        let mut configured = limits(100);
        configured.max_media_decoded_bytes = 3;
        configured.max_retained_bytes = 100;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        let url = "https://secure.notion-static.com/asset";
        fetch_media_bounded(&fetcher, url, 10, &budget).unwrap();
        let error = fetch_media_bounded(&fetcher, url, 10, &budget).unwrap_err();
        assert_eq!(
            error,
            InitialHydrationError::LimitExceeded {
                resource: HydrationResource::MediaDecodedBytes
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[derive(Debug)]
    struct ReservationTestFetcher {
        calls: Arc<AtomicUsize>,
        downloaded: Arc<AtomicUsize>,
        bytes: usize,
        first_admitted: Option<mpsc::Sender<usize>>,
        first_release: Option<Mutex<mpsc::Receiver<()>>>,
        failure: bool,
        panic: bool,
    }

    impl PortableMediaCaptureFetcher for ReservationTestFetcher {
        fn fetch(
            &self,
            _hosted_url: &str,
            _max_bytes: usize,
        ) -> LocalityResult<PortableMediaCapture> {
            unreachable!("bounded hook is required")
        }

        fn fetch_bounded(
            &self,
            _hosted_url: &str,
            max_bytes: usize,
            budget: &InitialHydrationBudget,
        ) -> InitialHydrationResult<PortableMediaCapture> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0
                && let Some(admitted) = &self.first_admitted
            {
                admitted.send(max_bytes).unwrap();
                self.first_release
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .recv()
                    .unwrap();
            }
            assert!(self.bytes <= max_bytes);
            if self.panic {
                panic!("scripted media fetch panic");
            }
            if self.failure {
                return Err(InitialHydrationError::ProviderUnavailable);
            }
            budget.account_response_chunk(self.bytes)?;
            self.downloaded.fetch_add(self.bytes, Ordering::SeqCst);
            Ok(PortableMediaCapture {
                bytes: vec![0; self.bytes],
                media_type: String::new(),
            })
        }
    }

    fn reservation_test_fetcher(
        bytes: usize,
        calls: Arc<AtomicUsize>,
        downloaded: Arc<AtomicUsize>,
    ) -> ReservationTestFetcher {
        ReservationTestFetcher {
            calls,
            downloaded,
            bytes,
            first_admitted: None,
            first_release: None,
            failure: false,
            panic: false,
        }
    }

    #[test]
    fn concurrent_media_fetches_cannot_share_one_remaining_byte_allowance() {
        let calls = Arc::new(AtomicUsize::new(0));
        let downloaded = Arc::new(AtomicUsize::new(0));
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let fetcher = Arc::new(ReservationTestFetcher {
            calls: Arc::clone(&calls),
            downloaded: Arc::clone(&downloaded),
            bytes: 4,
            first_admitted: Some(admitted_tx),
            first_release: Some(Mutex::new(release_rx)),
            failure: false,
            panic: false,
        });
        let mut configured = limits(100);
        configured.max_media_decoded_bytes = 10;
        configured.max_retained_bytes = 10;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        let first_budget = budget.clone();
        let first_fetcher = Arc::clone(&fetcher);
        let first = std::thread::spawn(move || {
            fetch_media_bounded(
                first_fetcher.as_ref(),
                "https://secure.notion-static.com/first",
                10,
                &first_budget,
            )
        });
        assert_eq!(admitted_rx.recv().unwrap(), 10);

        assert_eq!(
            fetch_media_bounded(
                fetcher.as_ref(),
                "https://secure.notion-static.com/second",
                10,
                &budget,
            ),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::MediaDecodedBytes
            })
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(downloaded.load(Ordering::SeqCst), 0);

        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap().unwrap().bytes.len(), 4);
        assert_eq!(
            budget.remaining(HydrationResource::MediaDecodedBytes),
            Ok(6)
        );
        assert_eq!(budget.remaining(HydrationResource::RetainedBytes), Ok(6));

        let final_fetcher =
            reservation_test_fetcher(6, Arc::clone(&calls), Arc::clone(&downloaded));
        assert_eq!(
            fetch_media_bounded(
                &final_fetcher,
                "https://secure.notion-static.com/final",
                10,
                &budget,
            )
            .unwrap()
            .bytes
            .len(),
            6
        );
        assert_eq!(downloaded.load(Ordering::SeqCst), 10);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            budget.remaining(HydrationResource::MediaDecodedBytes),
            Ok(0)
        );
        assert_eq!(budget.remaining(HydrationResource::RetainedBytes), Ok(0));
        assert_eq!(budget.remaining(HydrationResource::ProviderCalls), Ok(98));
        assert_eq!(budget.remaining(HydrationResource::MediaAssets), Ok(98));
    }

    #[test]
    fn media_fetch_error_releases_the_complete_byte_allowance() {
        let calls = Arc::new(AtomicUsize::new(0));
        let downloaded = Arc::new(AtomicUsize::new(0));
        let mut fetcher = reservation_test_fetcher(0, Arc::clone(&calls), Arc::clone(&downloaded));
        fetcher.failure = true;
        let mut configured = limits(100);
        configured.max_media_decoded_bytes = 7;
        configured.max_retained_bytes = 7;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        assert_eq!(
            fetch_media_bounded(
                &fetcher,
                "https://secure.notion-static.com/error",
                7,
                &budget,
            ),
            Err(InitialHydrationError::ProviderUnavailable)
        );
        assert_eq!(
            budget.remaining(HydrationResource::MediaDecodedBytes),
            Ok(7)
        );
        assert_eq!(budget.remaining(HydrationResource::RetainedBytes), Ok(7));
        assert_eq!(budget.remaining(HydrationResource::ProviderCalls), Ok(99));
        assert_eq!(budget.remaining(HydrationResource::MediaAssets), Ok(99));
    }

    #[test]
    fn media_short_body_releases_unused_byte_allowance() {
        let calls = Arc::new(AtomicUsize::new(0));
        let downloaded = Arc::new(AtomicUsize::new(0));
        let fetcher = reservation_test_fetcher(3, calls, downloaded);
        let mut configured = limits(100);
        configured.max_media_decoded_bytes = 7;
        configured.max_retained_bytes = 7;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        assert_eq!(
            fetch_media_bounded(
                &fetcher,
                "https://secure.notion-static.com/short",
                7,
                &budget,
            )
            .unwrap()
            .bytes
            .len(),
            3
        );
        assert_eq!(
            budget.remaining(HydrationResource::MediaDecodedBytes),
            Ok(4)
        );
        assert_eq!(budget.remaining(HydrationResource::RetainedBytes), Ok(4));
    }

    #[test]
    fn media_fetch_panic_releases_the_complete_byte_allowance() {
        let calls = Arc::new(AtomicUsize::new(0));
        let downloaded = Arc::new(AtomicUsize::new(0));
        let mut fetcher = reservation_test_fetcher(0, calls, downloaded);
        fetcher.panic = true;
        let mut configured = limits(100);
        configured.max_media_decoded_bytes = 7;
        configured.max_retained_bytes = 7;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = fetch_media_bounded(
                &fetcher,
                "https://secure.notion-static.com/panic",
                7,
                &budget,
            );
        }));
        assert!(result.is_err());
        assert_eq!(
            budget.remaining(HydrationResource::MediaDecodedBytes),
            Ok(7)
        );
        assert_eq!(budget.remaining(HydrationResource::RetainedBytes), Ok(7));
    }

    #[test]
    fn custom_media_result_is_revalidated_at_the_public_boundary() {
        struct OversizedFetcher;
        impl PortableMediaCaptureFetcher for OversizedFetcher {
            fn fetch(
                &self,
                _hosted_url: &str,
                _max_bytes: usize,
            ) -> LocalityResult<PortableMediaCapture> {
                unreachable!("bounded hook is required")
            }

            fn fetch_bounded(
                &self,
                _hosted_url: &str,
                max_bytes: usize,
                budget: &InitialHydrationBudget,
            ) -> InitialHydrationResult<PortableMediaCapture> {
                let bytes = vec![0; max_bytes + 1];
                budget.account_response_chunk(bytes.len())?;
                Ok(PortableMediaCapture {
                    bytes,
                    media_type: "image/png".to_string(),
                })
            }
        }

        let budget = InitialHydrationBudget::new(limits(100)).unwrap();
        assert_eq!(
            fetch_media_bounded(
                &OversizedFetcher,
                "https://secure.notion-static.com/asset",
                3,
                &budget,
            ),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::MediaDecodedBytes
            })
        );
        assert_eq!(
            budget.remaining(HydrationResource::ResponseBodyBytes),
            Ok(96)
        );
    }

    #[test]
    fn custom_media_return_is_rejected_after_the_absolute_deadline() {
        struct SlowFetcher;
        impl PortableMediaCaptureFetcher for SlowFetcher {
            fn fetch(
                &self,
                _hosted_url: &str,
                _max_bytes: usize,
            ) -> LocalityResult<PortableMediaCapture> {
                unreachable!("bounded hook is required")
            }

            fn fetch_bounded(
                &self,
                _hosted_url: &str,
                _max_bytes: usize,
                _budget: &InitialHydrationBudget,
            ) -> InitialHydrationResult<PortableMediaCapture> {
                std::thread::sleep(std::time::Duration::from_millis(3));
                Ok(PortableMediaCapture {
                    bytes: vec![1],
                    media_type: "image/png".to_string(),
                })
            }
        }

        let mut configured = limits(100);
        configured.provider_deadline_ms = 1;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        assert_eq!(
            fetch_media_bounded(
                &SlowFetcher,
                "https://secure.notion-static.com/asset",
                3,
                &budget,
            ),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::ProviderDeadline
            })
        );
        assert_eq!(
            budget.remaining(HydrationResource::MediaDecodedBytes),
            Ok(100)
        );
    }

    #[test]
    fn real_renderer_accounts_the_complete_shadow_representation() {
        let entity = NativeEntity {
            remote_id: RemoteId::new("page"),
            kind: "notion_page".to_string(),
            raw: serde_json::to_vec(&NotionPageBundle {
                page: PageDto {
                    id: "page".to_string(),
                    parent: None,
                    created_time: None,
                    last_edited_time: None,
                    archived: false,
                    in_trash: false,
                    properties: Default::default(),
                },
                blocks: Vec::new(),
            })
            .unwrap(),
        };
        let rendered = render_native_entity(&entity).unwrap();
        let retained = encoded_len(&rendered).unwrap() as u64;
        let content = render_canonical_markdown(&rendered.document).len() as u64;
        assert!(retained > content);

        let mut exact = limits(retained.max(entity.raw.len() as u64).max(1));
        exact.max_retained_bytes = retained;
        exact.max_rendered_content_bytes = content.max(1);
        render_native_entity_bounded(&entity, &InitialHydrationBudget::new(exact).unwrap())
            .unwrap();

        let mut short = exact;
        short.max_retained_bytes = retained - 1;
        assert_eq!(
            render_native_entity_bounded(&entity, &InitialHydrationBudget::new(short).unwrap(),),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RetainedBytes
            })
        );
    }

    #[test]
    fn real_portable_renderer_accepts_exact_retained_cap_and_rejects_one_short() {
        let request = PortableRenderRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            logical_path: LogicalPath::new("Roadmap/page.md").unwrap(),
            native: NativeEntity {
                remote_id: RemoteId::new("page"),
                kind: "notion_page".to_string(),
                raw: serde_json::to_vec(&NotionPageBundle {
                    page: PageDto {
                        id: "page".to_string(),
                        parent: None,
                        created_time: None,
                        last_edited_time: None,
                        archived: false,
                        in_trash: false,
                        properties: Default::default(),
                    },
                    blocks: Vec::new(),
                })
                .unwrap(),
            },
            format_version: 1,
        };
        let expected = crate::portable::render(&request).unwrap();
        let retained = encoded_len(&expected).unwrap() as u64;
        let content = expected
            .projections
            .iter()
            .fold(expected.canonical.body.len(), |total, projection| {
                total + projection.artifact.body.len()
            }) as u64;
        assert!(retained > content);
        assert!(retained > request.native.raw.len() as u64);

        let mut exact = limits(retained);
        exact.max_rendered_content_bytes = content.max(1);
        assert_eq!(
            render_portable_bounded(&request, &InitialHydrationBudget::new(exact).unwrap(),)
                .unwrap(),
            expected
        );

        let mut short = exact;
        short.max_retained_bytes = retained - 1;
        assert_eq!(
            render_portable_bounded(&request, &InitialHydrationBudget::new(short).unwrap(),),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RetainedBytes
            })
        );
    }

    #[test]
    fn rendered_projection_and_shared_retained_caps_are_enforced() {
        let mut configured = limits(10);
        configured.max_retained_bytes = 5;
        let budget = InitialHydrationBudget::new(configured).unwrap();
        assert_eq!(
            budget.account_render_output(5, 1, 6),
            Err(InitialHydrationError::LimitExceeded {
                resource: HydrationResource::RetainedBytes
            })
        );
        assert_eq!(
            budget.remaining(HydrationResource::RenderedContentBytes),
            Ok(10)
        );
        budget.account_render_output(5, 1, 5).unwrap();
        assert_eq!(budget.remaining(HydrationResource::Projections), Ok(9));
    }
}
