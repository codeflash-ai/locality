use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use locality_connector::hydration_budget::{
    InitialHydrationBudget, InitialHydrationError, InitialHydrationLimits,
};
use locality_connector::{Connector, PortableBootstrapRequest, PortableSourceScope};
use locality_core::model::RemoteId;
use locality_core::portable::SourceConnectionId;
use locality_core::{LocalityError, LocalityResult};
use locality_engine::synchronize_project::{
    BootstrapAggregationLimits, bootstrap_and_project, bootstrap_and_project_to_completion,
};
use locality_notion::client::NotionApi;
use locality_notion::dto::{
    BlockDto, BlockListDto, DataSourceDto, DataSourceSummaryDto, DatabaseDto, FileBlockDto,
    HostedFileDto, PageDto, PageListDto, PagePropertyDto, PaginatedListDto, ParentDto,
    TitleBlockDto,
};
use locality_notion::media::{
    PortableMediaCapture, PortableMediaCaptureFetcher, PortableMediaCapturePolicy,
};
use locality_notion::{NotionConfig, NotionConnector};

const ROOT: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const CHILD_1: &str = "11111111-1111-1111-1111-111111111111";
const CHILD_2: &str = "22222222-2222-2222-2222-222222222222";
const CHILD_3: &str = "33333333-3333-3333-3333-333333333333";
const CHILD_4: &str = "44444444-4444-4444-4444-444444444444";
const CONNECTION_HASH: &str =
    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const DATABASE_ROOT: &str = "dddddddd-dddd-dddd-dddd-dddddddddddd";
const DATA_SOURCE: &str = "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee";
const DATABASE_ROW: &str = "99999999-9999-9999-9999-999999999999";
const FOREIGN_DATABASE: &str = "ffffffff-ffff-ffff-ffff-ffffffffffff";
const FOREIGN_DATA_SOURCE: &str = "abababab-abab-abab-abab-abababababab";

#[derive(Debug)]
struct FixtureApi {
    pages: Mutex<BTreeMap<String, PageDto>>,
    root_children: Mutex<Vec<String>>,
    root_extra_blocks: Mutex<Vec<BlockDto>>,
    calls: Mutex<Vec<String>>,
}

impl FixtureApi {
    fn new(children: &[&str]) -> Self {
        let mut pages = BTreeMap::new();
        pages.insert(canonical(ROOT), page(ROOT, None, "root-v1"));
        for child in children {
            pages.insert(
                canonical(child),
                page(child, Some(ROOT), &format!("{child}-v1")),
            );
        }
        Self {
            pages: Mutex::new(pages),
            root_children: Mutex::new(children.iter().map(|id| id.to_string()).collect()),
            root_extra_blocks: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn with_root_blocks(self, blocks: Vec<BlockDto>) -> Self {
        *self.root_extra_blocks.lock().expect("root blocks") = blocks;
        self
    }

    fn add_child(&self, child: &str) {
        self.pages.lock().expect("pages").insert(
            canonical(child),
            page(child, Some(ROOT), &format!("{child}-v1")),
        );
        self.root_children
            .lock()
            .expect("children")
            .push(child.to_string());
    }

    fn mutate_page_version(&self, page_id: &str, version: &str) {
        self.pages
            .lock()
            .expect("pages")
            .get_mut(&canonical(page_id))
            .expect("page")
            .last_edited_time = Some(version.to_string());
    }

    fn record(&self, call: impl Into<String>) {
        self.calls.lock().expect("calls").push(call.into());
    }

    fn call_count(&self, prefix: &str) -> usize {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|call| call.starts_with(prefix))
            .count()
    }

    fn page_result(&self, page_id: &str) -> LocalityResult<PageDto> {
        self.pages
            .lock()
            .expect("pages")
            .get(&canonical(page_id))
            .cloned()
            .ok_or_else(|| LocalityError::RemoteNotFound("redacted".to_string()))
    }

    fn children_result(&self, block_id: &str) -> BlockListDto {
        let mut results = if canonical(block_id) == canonical(ROOT) {
            self.root_children
                .lock()
                .expect("children")
                .iter()
                .map(|id| BlockDto {
                    id: id.clone(),
                    kind: "child_page".to_string(),
                    child_page: Some(TitleBlockDto {
                        title: "Collision".to_string(),
                    }),
                    ..Default::default()
                })
                .collect()
        } else {
            Vec::new()
        };
        if canonical(block_id) == canonical(ROOT) {
            results.extend(self.root_extra_blocks.lock().expect("root blocks").clone());
        }
        PaginatedListDto {
            results,
            next_cursor: None,
            has_more: false,
        }
    }

    fn bounded<T: serde::Serialize>(
        &self,
        value: T,
        budget: &InitialHydrationBudget,
    ) -> Result<T, InitialHydrationError> {
        budget.reserve_provider_call()?;
        let bytes = serde_json::to_vec(&value)
            .map_err(|_| InitialHydrationError::ProviderResponseInvalid)?;
        budget.account_response_chunk(bytes.len())?;
        Ok(value)
    }
}

#[derive(Debug)]
struct BoundedMediaFetcher {
    calls: Arc<Mutex<Vec<String>>>,
}

impl PortableMediaCaptureFetcher for BoundedMediaFetcher {
    fn fetch(&self, _hosted_url: &str, _max_bytes: usize) -> LocalityResult<PortableMediaCapture> {
        panic!("session media must use the bounded hook")
    }

    fn fetch_bounded(
        &self,
        hosted_url: &str,
        max_bytes: usize,
        budget: &InitialHydrationBudget,
    ) -> Result<PortableMediaCapture, InitialHydrationError> {
        self.calls
            .lock()
            .expect("media calls")
            .push(hosted_url.to_string());
        let bytes = vec![0x89, b'P', b'N', b'G'];
        assert!(bytes.len() <= max_bytes);
        budget.account_response_chunk(bytes.len())?;
        Ok(PortableMediaCapture {
            bytes,
            media_type: "image/png".to_string(),
        })
    }
}

impl NotionApi for FixtureApi {
    fn retrieve_page(&self, page_id: &str) -> LocalityResult<PageDto> {
        self.record(format!("plain:page:{page_id}"));
        self.page_result(page_id)
    }

    fn retrieve_database(&self, database_id: &str) -> LocalityResult<DatabaseDto> {
        self.record(format!("plain:database:{database_id}"));
        Err(LocalityError::RemoteNotFound("redacted".to_string()))
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        _start_cursor: Option<&str>,
    ) -> LocalityResult<BlockListDto> {
        self.record(format!("plain:children:{block_id}"));
        Ok(self.children_result(block_id))
    }

    fn search_pages(&self, _start_cursor: Option<&str>) -> LocalityResult<PageListDto> {
        panic!("explicit-root session must not search")
    }

    fn update_block(&self, _block_id: &str, _body: serde_json::Value) -> LocalityResult<BlockDto> {
        panic!("read-only fixture")
    }

    fn append_block_children(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> LocalityResult<BlockListDto> {
        panic!("read-only fixture")
    }

    fn delete_block(&self, _block_id: &str) -> LocalityResult<BlockDto> {
        panic!("read-only fixture")
    }

    fn retrieve_page_bounded(
        &self,
        page_id: &str,
        budget: &InitialHydrationBudget,
    ) -> Result<PageDto, InitialHydrationError> {
        self.record(format!("bounded:page:{page_id}"));
        let page = self.page_result(page_id).map_err(|error| match error {
            LocalityError::RemoteNotFound(_) => InitialHydrationError::ProviderNotFound,
            _ => InitialHydrationError::ProviderResponseInvalid,
        })?;
        self.bounded(page, budget)
    }

    fn retrieve_block_children_bounded(
        &self,
        block_id: &str,
        _start_cursor: Option<&str>,
        budget: &InitialHydrationBudget,
    ) -> Result<BlockListDto, InitialHydrationError> {
        self.record(format!("bounded:children:{block_id}"));
        self.bounded(self.children_result(block_id), budget)
    }
}

#[derive(Debug)]
struct DatabaseFixtureApi {
    row_parent: Option<ParentDto>,
    row_padding: usize,
    calls: Mutex<Vec<String>>,
}

impl DatabaseFixtureApi {
    fn new(row_parent: Option<ParentDto>) -> Self {
        Self {
            row_parent,
            row_padding: 0,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn with_row_padding(mut self, row_padding: usize) -> Self {
        self.row_padding = row_padding;
        self
    }

    fn record(&self, call: impl Into<String>) {
        self.calls.lock().expect("calls").push(call.into());
    }

    fn call_count(&self, prefix: &str) -> usize {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|call| call.starts_with(prefix))
            .count()
    }

    fn database(&self) -> DatabaseDto {
        DatabaseDto {
            id: DATABASE_ROOT.to_string(),
            last_edited_time: Some("database-v1".to_string()),
            data_sources: vec![DataSourceSummaryDto {
                id: DATA_SOURCE.to_string(),
                name: Some("Rows".to_string()),
            }],
            ..Default::default()
        }
    }

    fn data_source(&self) -> DataSourceDto {
        DataSourceDto {
            id: DATA_SOURCE.to_string(),
            parent: Some(ParentDto {
                kind: "database_id".to_string(),
                database_id: Some(DATABASE_ROOT.to_string()),
                ..Default::default()
            }),
            last_edited_time: Some("data-source-v1".to_string()),
            ..Default::default()
        }
    }

    fn row(&self) -> PageDto {
        let mut properties = BTreeMap::new();
        if self.row_padding > 0 {
            properties.insert(
                "padding".to_string(),
                PagePropertyDto {
                    kind: "x".repeat(self.row_padding),
                    ..Default::default()
                },
            );
        }
        PageDto {
            id: DATABASE_ROW.to_string(),
            parent: self.row_parent.clone(),
            created_time: None,
            last_edited_time: Some("row-v1".to_string()),
            archived: false,
            in_trash: false,
            properties,
        }
    }

    fn bounded<T: serde::Serialize>(
        &self,
        value: T,
        budget: &InitialHydrationBudget,
    ) -> Result<T, InitialHydrationError> {
        budget.reserve_provider_call()?;
        budget.account_response_chunk(
            serde_json::to_vec(&value)
                .map_err(|_| InitialHydrationError::ProviderResponseInvalid)?
                .len(),
        )?;
        Ok(value)
    }
}

impl NotionApi for DatabaseFixtureApi {
    fn retrieve_page(&self, page_id: &str) -> LocalityResult<PageDto> {
        if canonical(page_id) == canonical(DATABASE_ROW) {
            Ok(self.row())
        } else {
            Err(LocalityError::RemoteNotFound("redacted".to_string()))
        }
    }

    fn retrieve_database(&self, database_id: &str) -> LocalityResult<DatabaseDto> {
        (canonical(database_id) == canonical(DATABASE_ROOT))
            .then(|| self.database())
            .ok_or_else(|| LocalityError::RemoteNotFound("redacted".to_string()))
    }

    fn retrieve_data_source(&self, data_source_id: &str) -> LocalityResult<DataSourceDto> {
        (canonical(data_source_id) == canonical(DATA_SOURCE))
            .then(|| self.data_source())
            .ok_or_else(|| LocalityError::RemoteNotFound("redacted".to_string()))
    }

    fn query_data_source(
        &self,
        data_source_id: &str,
        _start_cursor: Option<&str>,
    ) -> LocalityResult<PageListDto> {
        if canonical(data_source_id) != canonical(DATA_SOURCE) {
            return Err(LocalityError::RemoteNotFound("redacted".to_string()));
        }
        Ok(PageListDto {
            results: vec![self.row()],
            next_cursor: None,
            has_more: false,
        })
    }

    fn retrieve_block_children(
        &self,
        _block_id: &str,
        _start_cursor: Option<&str>,
    ) -> LocalityResult<BlockListDto> {
        Ok(BlockListDto {
            results: Vec::new(),
            next_cursor: None,
            has_more: false,
        })
    }

    fn search_pages(&self, _start_cursor: Option<&str>) -> LocalityResult<PageListDto> {
        panic!("explicit-root session must not search")
    }

    fn update_block(&self, _block_id: &str, _body: serde_json::Value) -> LocalityResult<BlockDto> {
        panic!("read-only fixture")
    }

    fn append_block_children(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> LocalityResult<BlockListDto> {
        panic!("read-only fixture")
    }

    fn delete_block(&self, _block_id: &str) -> LocalityResult<BlockDto> {
        panic!("read-only fixture")
    }

    fn retrieve_page_bounded(
        &self,
        page_id: &str,
        budget: &InitialHydrationBudget,
    ) -> Result<PageDto, InitialHydrationError> {
        self.record(format!("bounded:page:{page_id}"));
        budget.reserve_provider_call()?;
        if canonical(page_id) != canonical(DATABASE_ROW) {
            return Err(InitialHydrationError::ProviderNotFound);
        }
        let row = self.row();
        budget.account_response_chunk(
            serde_json::to_vec(&row)
                .map_err(|_| InitialHydrationError::ProviderResponseInvalid)?
                .len(),
        )?;
        Ok(row)
    }

    fn retrieve_database_bounded(
        &self,
        database_id: &str,
        budget: &InitialHydrationBudget,
    ) -> Result<DatabaseDto, InitialHydrationError> {
        self.record(format!("bounded:database:{database_id}"));
        if canonical(database_id) != canonical(DATABASE_ROOT) {
            return Err(InitialHydrationError::ProviderNotFound);
        }
        self.bounded(self.database(), budget)
    }

    fn retrieve_data_source_bounded(
        &self,
        data_source_id: &str,
        budget: &InitialHydrationBudget,
    ) -> Result<DataSourceDto, InitialHydrationError> {
        self.record(format!("bounded:data-source:{data_source_id}"));
        if canonical(data_source_id) != canonical(DATA_SOURCE) {
            return Err(InitialHydrationError::ProviderNotFound);
        }
        self.bounded(self.data_source(), budget)
    }

    fn query_data_source_bounded(
        &self,
        data_source_id: &str,
        _start_cursor: Option<&str>,
        budget: &InitialHydrationBudget,
    ) -> Result<PageListDto, InitialHydrationError> {
        self.record(format!("bounded:query:{data_source_id}"));
        if canonical(data_source_id) != canonical(DATA_SOURCE) {
            return Err(InitialHydrationError::ProviderNotFound);
        }
        self.bounded(
            PageListDto {
                results: vec![self.row()],
                next_cursor: None,
                has_more: false,
            },
            budget,
        )
    }

    fn retrieve_block_children_bounded(
        &self,
        block_id: &str,
        _start_cursor: Option<&str>,
        budget: &InitialHydrationBudget,
    ) -> Result<BlockListDto, InitialHydrationError> {
        self.record(format!("bounded:children:{block_id}"));
        self.bounded(
            BlockListDto {
                results: Vec::new(),
                next_cursor: None,
                has_more: false,
            },
            budget,
        )
    }
}

#[test]
fn one_inventory_drains_many_pages_and_each_source_fetches_once() {
    let api = Arc::new(FixtureApi::new(&[CHILD_1, CHILD_2, CHILD_3]));
    let connector = connector(api.clone());
    let session = connector
        .initial_hydration_session(CONNECTION_HASH, 1, limits(10_000))
        .expect("session");

    let aggregate = bootstrap_and_project_to_completion(
        &session,
        request(None, 100, "connection"),
        1,
        BootstrapAggregationLimits {
            max_checkpoints: 10,
            max_total_changes: 10,
            max_total_content_bytes: 1_000_000,
        },
    )
    .expect("aggregate");

    assert_eq!(aggregate.observed_changes.len(), 4);
    assert!(aggregate.is_publication_eligible());
    let projected_paths = aggregate
        .projections
        .iter()
        .map(|projection| projection.logical_path.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(projected_paths.len(), 4);
    assert_eq!(
        projected_paths
            .iter()
            .filter(|path| path.contains("/Collision "))
            .count(),
        3,
        "the bounded inventory preserves collision-safe projection"
    );
    // Root inventory traverses its children once. The second root-children call
    // is the root's one native fetch; pagination never re-enumerates it.
    assert_eq!(api.call_count("bounded:children:"), 8);
    for id in [ROOT, CHILD_1, CHILD_2, CHILD_3] {
        assert_eq!(
            api.call_count(&format!("bounded:page:{id}")),
            2,
            "one metadata inventory read plus one native fetch"
        );
    }
    assert_eq!(aggregate.next_checkpoint.format_version, 2);
    connector
        .sync_portable(locality_connector::PortableSyncRequest {
            source_connection_id: SourceConnectionId::new("connection"),
            scope: scope(),
            checkpoint: aggregate.next_checkpoint,
            hints: Vec::new(),
            max_changes: 100,
        })
        .expect("durable terminal checkpoint is accepted by normal sync");
}

#[test]
fn overlapping_parent_and_child_roots_are_disjoint_bounded_scopes() {
    let api = Arc::new(FixtureApi::new(&[CHILD_1]));
    let connector =
        connector_with_api(api).with_root_ids([RemoteId::new(ROOT), RemoteId::new(CHILD_1)]);
    let session = connector
        .initial_hydration_session(CONNECTION_HASH, 10, limits(10_000))
        .expect("session");
    let batch = session
        .bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: SourceConnectionId::new("connection"),
            scope: PortableSourceScope::explicit_roots([
                RemoteId::new(ROOT),
                RemoteId::new(CHILD_1),
            ]),
            checkpoint: None,
            max_changes: 10,
        })
        .expect("overlapping roots partition without duplicate inventory");

    assert_eq!(batch.changes.len(), 2);
    assert_eq!(
        batch
            .changes
            .iter()
            .map(|change| (
                change.source_object.remote_id.as_str(),
                change.logical_path.as_ref().expect("path").as_str(),
                change.source_object.edges[0].target_remote_id.as_str(),
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                CHILD_1,
                "Untitled 111111/page.md",
                "11111111111111111111111111111111",
            ),
            (
                ROOT,
                "Untitled aaaaaa/page.md",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
        ]
    );
}

#[test]
fn overlapping_database_and_selected_row_are_disjoint_bounded_scopes() {
    let api = Arc::new(DatabaseFixtureApi::new(Some(ParentDto {
        kind: "data_source_id".to_string(),
        data_source_id: Some(DATA_SOURCE.to_string()),
        ..Default::default()
    })));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone())
        .with_root_ids([RemoteId::new(DATABASE_ROOT), RemoteId::new(DATABASE_ROW)]);
    let session = connector
        .initial_hydration_session(CONNECTION_HASH, 10, limits(10_000))
        .expect("session");
    let batch = session
        .bootstrap_portable(PortableBootstrapRequest {
            source_connection_id: SourceConnectionId::new("connection"),
            scope: PortableSourceScope::explicit_roots([
                RemoteId::new(DATABASE_ROOT),
                RemoteId::new(DATABASE_ROW),
            ]),
            checkpoint: None,
            max_changes: 10,
        })
        .expect("database and selected row partition without duplicate inventory");

    assert_eq!(batch.changes.len(), 2);
    for change in &batch.changes {
        assert_eq!(change.source_object.edges.len(), 1);
        assert_eq!(
            canonical(change.source_object.remote_id.as_str()),
            canonical(change.source_object.edges[0].target_remote_id.as_str()),
        );
    }
    assert_eq!(api.call_count("bounded:query:"), 1);
    assert_eq!(
        api.call_count(&format!("bounded:page:{DATABASE_ROW}")),
        1,
        "the selected row is read once as its own root, not again under the database",
    );
}

#[test]
fn checkpoints_are_session_bound_ordered_redacted_and_rejected_by_base_connector() {
    let api = Arc::new(FixtureApi::new(&[CHILD_1, CHILD_2]));
    let connector = connector(api);
    let session = connector
        .initial_hydration_session(CONNECTION_HASH, 1, limits(10_000))
        .expect("session");
    let first = session
        .bootstrap_portable(request(None, 10, "connection"))
        .expect("first page");
    assert_eq!(first.next_checkpoint.format_version, 4);
    let checkpoint_json: serde_json::Value =
        serde_json::from_str(&first.next_checkpoint.opaque).expect("checkpoint JSON");
    assert_eq!(
        checkpoint_json
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "canonical_root_set_sha256",
            "component_version",
            "inventory_sha256",
            "next_index",
            "session_nonce",
            "source_connection_identity_sha256",
        ]
    );
    assert!(!first.next_checkpoint.opaque.contains("https://"));
    assert!(!first.next_checkpoint.opaque.contains("token"));
    assert!(!format!("{session:?}").contains(CONNECTION_HASH));
    connector
        .bootstrap_portable(request(
            Some(first.next_checkpoint.clone()),
            10,
            "connection",
        ))
        .expect_err("ephemeral checkpoint must fail through base connector");

    let other = connector
        .initial_hydration_session(CONNECTION_HASH, 1, limits(10_000))
        .expect("other session");
    other
        .bootstrap_portable(request(
            Some(first.next_checkpoint.clone()),
            10,
            "connection",
        ))
        .expect_err("fresh and cross-session checkpoint");

    let second = session
        .bootstrap_portable(request(
            Some(first.next_checkpoint.clone()),
            10,
            "connection",
        ))
        .expect("second page");
    assert_eq!(second.changes.len(), 1);
    session
        .bootstrap_portable(request(Some(first.next_checkpoint), 10, "connection"))
        .expect_err("replayed checkpoint");
}

#[test]
fn wrong_connection_root_and_skipped_checkpoint_fail_closed() {
    let connector = connector(Arc::new(FixtureApi::new(&[CHILD_1, CHILD_2])));

    let wrong_connection = connector
        .initial_hydration_session(CONNECTION_HASH, 1, limits(10_000))
        .expect("session");
    let first = wrong_connection
        .bootstrap_portable(request(None, 10, "connection"))
        .expect("first");
    wrong_connection
        .bootstrap_portable(request(
            Some(first.next_checkpoint),
            10,
            "different-connection",
        ))
        .expect_err("wrong connection");

    let wrong_root = connector
        .initial_hydration_session(CONNECTION_HASH, 1, limits(10_000))
        .expect("session");
    let mut wrong_root_request = request(None, 10, "connection");
    wrong_root_request.scope = PortableSourceScope::explicit_roots([RemoteId::new(CHILD_1)]);
    wrong_root
        .bootstrap_portable(wrong_root_request)
        .expect_err("wrong root");

    let skipped = connector
        .initial_hydration_session(CONNECTION_HASH, 1, limits(10_000))
        .expect("session");
    let first = skipped
        .bootstrap_portable(request(None, 10, "connection"))
        .expect("first");
    let mut tampered = first.next_checkpoint;
    let mut value: serde_json::Value = serde_json::from_str(&tampered.opaque).expect("JSON");
    value["next_index"] = serde_json::json!(999);
    tampered.opaque = serde_json::to_string(&value).expect("JSON");
    skipped
        .bootstrap_portable(request(Some(tampered), 10, "connection"))
        .expect_err("skipped checkpoint");
}

#[test]
fn provider_mutation_is_deferred_until_the_next_normal_sync() {
    let api = Arc::new(FixtureApi::new(&[CHILD_1, CHILD_2]));
    let connector = connector(api.clone());
    let session = connector
        .initial_hydration_session(CONNECTION_HASH, 1, limits(10_000))
        .expect("session");
    let first = bootstrap_and_project(&session, request(None, 10, "connection"), 1)
        .expect("first projected page");
    api.add_child(CHILD_4);

    let mut checkpoint = Some(first.next_checkpoint);
    let mut seen = first.observed_changes;
    loop {
        let page = bootstrap_and_project(&session, request(checkpoint.take(), 10, "connection"), 1)
            .expect("continued projected page");
        let continuation = page
            .completeness
            .incomplete_reasons()
            .contains(&locality_connector::PortableIncompleteReason::CheckpointContinuation);
        checkpoint = Some(page.next_checkpoint);
        seen.extend(page.observed_changes);
        if !continuation {
            break;
        }
    }
    assert!(
        !seen
            .iter()
            .any(|change| change.source_object.remote_id == RemoteId::new(CHILD_4))
    );

    let synchronized = connector
        .sync_portable(locality_connector::PortableSyncRequest {
            source_connection_id: SourceConnectionId::new("connection"),
            scope: scope(),
            checkpoint: checkpoint.expect("terminal checkpoint"),
            hints: Vec::new(),
            max_changes: 100,
        })
        .expect("next sync");
    assert!(
        synchronized
            .changes
            .iter()
            .any(|change| change.source_object.remote_id == RemoteId::new(CHILD_4))
    );
}

#[test]
fn bounded_media_keeps_checkpoint_and_projected_outputs_redacted() {
    let signed_url = concat!(
        "https://secure.notion-static.com/assets/image.png?",
        "X-Amz-Signature=signature-secret&token=token-secret"
    );
    let image = BlockDto {
        id: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string(),
        kind: "image".to_string(),
        image: Some(FileBlockDto {
            kind: "file".to_string(),
            file: Some(HostedFileDto {
                url: signed_url.to_string(),
                expiry_time: Some("2099-08-13T00:00:00.000Z".to_string()),
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let api = Arc::new(FixtureApi::new(&[]).with_root_blocks(vec![image]));
    let media_calls = Arc::new(Mutex::new(Vec::new()));
    let connector = connector(api).with_portable_media_capture_fetcher(
        PortableMediaCapturePolicy::HostedPilot,
        Arc::new(BoundedMediaFetcher {
            calls: Arc::clone(&media_calls),
        }),
    );
    let session = connector
        .initial_hydration_session(CONNECTION_HASH, 10, limits(10_000))
        .expect("session");
    let aggregate = bootstrap_and_project_to_completion(
        &session,
        request(None, 10, "connection"),
        1,
        BootstrapAggregationLimits {
            max_checkpoints: 2,
            max_total_changes: 2,
            max_total_content_bytes: 1_000_000,
        },
    )
    .expect("media aggregate");

    assert_eq!(media_calls.lock().expect("calls").as_slice(), [signed_url]);
    assert_eq!(aggregate.next_checkpoint.format_version, 2);
    assert!(!aggregate.next_checkpoint.opaque.contains("https://"));
    assert!(!aggregate.next_checkpoint.opaque.contains("secret"));
    for content in &aggregate.contents {
        assert!(
            !content
                .body
                .windows(b"secret".len())
                .any(|part| part == b"secret")
        );
    }
    for projection in &aggregate.projections {
        assert!(!projection.logical_path.as_str().contains("secret"));
    }
}

#[test]
fn failure_restart_gets_a_new_nonce_and_limits_publish_no_checkpoint() {
    let api = Arc::new(FixtureApi::new(&[CHILD_1, CHILD_2]));
    let connector = connector(api.clone());
    let first_session = connector
        .initial_hydration_session(CONNECTION_HASH, 1, limits(10_000))
        .expect("session");
    let first = first_session
        .bootstrap_portable(request(None, 10, "connection"))
        .expect("first");
    first_session
        .bootstrap_portable(request(None, 10, "connection"))
        .expect_err("missing active checkpoint");

    let restarted = connector
        .initial_hydration_session(CONNECTION_HASH, 1, limits(10_000))
        .expect("restart");
    let restarted_first = restarted
        .bootstrap_portable(request(None, 10, "connection"))
        .expect("restarted first");
    assert_ne!(first.next_checkpoint, restarted_first.next_checkpoint);
    assert_eq!(api.call_count("bounded:page:"), 6);

    let mut tight = limits(10_000);
    tight.max_inventory_items = 1;
    let limited_api = Arc::new(FixtureApi::new(&[CHILD_1]));
    let limited = connector_with_api(limited_api.clone())
        .with_root_ids([RemoteId::new(ROOT)])
        .initial_hydration_session(CONNECTION_HASH, 1, tight)
        .expect("limited session");
    limited
        .bootstrap_portable(request(None, 10, "connection"))
        .expect_err("inventory limit");
    assert_eq!(limited_api.call_count("bounded:page:"), 1);
    assert_eq!(limited_api.call_count("bounded:children:"), 0);

    let change_limited_api = Arc::new(FixtureApi::new(&[CHILD_1, CHILD_2]));
    let mut change_limits = limits(10_000);
    change_limits.max_changes = 1;
    let change_limited = connector_with_api(change_limited_api.clone())
        .with_root_ids([RemoteId::new(ROOT)])
        .initial_hydration_session(CONNECTION_HASH, 100, change_limits)
        .expect("change-limited session");
    let first = change_limited
        .bootstrap_portable(request(None, 100, "connection"))
        .expect("one remaining change");
    assert_eq!(first.changes.len(), 1);
    let calls_after_inventory = change_limited_api.call_count("bounded:");
    change_limited
        .bootstrap_portable(request(Some(first.next_checkpoint), 100, "connection"))
        .expect_err("exhausted change budget publishes no next checkpoint");
    assert_eq!(
        change_limited_api.call_count("bounded:"),
        calls_after_inventory,
        "change exhaustion prevents further provider work"
    );
}

#[test]
fn inventory_version_change_fails_before_render_or_terminal_checkpoint() {
    let api = Arc::new(FixtureApi::new(&[CHILD_1]));
    let connector = connector(api.clone());
    let session = connector
        .initial_hydration_session(CONNECTION_HASH, 1, limits(10_000))
        .expect("session");
    let first = session
        .bootstrap_portable(request(None, 10, "connection"))
        .expect("inventory page");
    assert_eq!(first.next_checkpoint.format_version, 4);
    let change = first.changes.first().expect("emitted change");
    api.mutate_page_version(
        change.source_object.remote_id.as_str(),
        "renamed-and-edited-v2",
    );

    session
        .fetch_portable(locality_connector::PortableFetchRequest {
            source_connection_id: SourceConnectionId::new("connection"),
            remote_id: change.source_object.remote_id.clone(),
            reason: locality_connector::PortableFetchReason::Bootstrap,
        })
        .expect_err("changed provider version must invalidate the job");
    session
        .bootstrap_portable(request(Some(first.next_checkpoint), 10, "connection"))
        .expect_err("failed session cannot mint a terminal checkpoint");
}

#[test]
fn session_capabilities_describe_only_the_implemented_read_pipeline() {
    let session = connector(Arc::new(FixtureApi::new(&[])))
        .initial_hydration_session(CONNECTION_HASH, 1, limits(10_000))
        .expect("session");
    assert_eq!(
        session.capabilities(),
        locality_connector::ConnectorCapabilities {
            supports_databases: true,
            ..Default::default()
        }
    );
    assert!(session.supported_push_operations().is_empty());
}

#[test]
fn database_root_exact_row_parent_completes_with_bound_versions() {
    let api = Arc::new(DatabaseFixtureApi::new(Some(ParentDto {
        kind: "data_source_id".to_string(),
        data_source_id: Some(DATA_SOURCE.to_string()),
        database_id: Some(DATABASE_ROOT.to_string()),
        ..Default::default()
    })));
    let connector = connector_with_database_api(api.clone());
    let session = connector
        .initial_hydration_session(CONNECTION_HASH, 10, limits(100_000))
        .expect("session");
    let aggregate = bootstrap_and_project_to_completion(
        &session,
        request_for_root(None, 10, "connection", DATABASE_ROOT),
        1,
        BootstrapAggregationLimits {
            max_checkpoints: 2,
            max_total_changes: 2,
            max_total_content_bytes: 1_000_000,
        },
    )
    .expect("database aggregate");

    assert_eq!(aggregate.observed_changes.len(), 2);
    assert_eq!(aggregate.next_checkpoint.format_version, 2);
    assert!(aggregate.is_publication_eligible());
    assert_eq!(api.call_count("bounded:query:"), 1);
    connector
        .sync_portable(locality_connector::PortableSyncRequest {
            source_connection_id: SourceConnectionId::new("connection"),
            scope: PortableSourceScope::explicit_roots([RemoteId::new(DATABASE_ROOT)]),
            checkpoint: aggregate.next_checkpoint,
            hints: Vec::new(),
            max_changes: 10,
        })
        .expect("database terminal checkpoint is accepted by normal sync");
}

#[test]
fn database_root_rejects_unowned_or_ambiguous_query_rows_before_processing() {
    let cases = [
        ("parentless", None),
        (
            "absent IDs",
            Some(ParentDto {
                kind: "data_source_id".to_string(),
                ..Default::default()
            }),
        ),
        (
            "foreign data source",
            Some(ParentDto {
                kind: "data_source_id".to_string(),
                data_source_id: Some(FOREIGN_DATA_SOURCE.to_string()),
                ..Default::default()
            }),
        ),
        (
            "foreign database",
            Some(ParentDto {
                kind: "data_source_id".to_string(),
                data_source_id: Some(DATA_SOURCE.to_string()),
                database_id: Some(FOREIGN_DATABASE.to_string()),
                ..Default::default()
            }),
        ),
        (
            "ambiguous parent",
            Some(ParentDto {
                kind: "data_source_id".to_string(),
                data_source_id: Some(DATA_SOURCE.to_string()),
                page_id: Some(ROOT.to_string()),
                ..Default::default()
            }),
        ),
    ];

    for (name, parent) in cases {
        let api = Arc::new(DatabaseFixtureApi::new(parent));
        let connector = connector_with_database_api(api.clone());
        let session = connector
            .initial_hydration_session(CONNECTION_HASH, 10, limits(100_000))
            .expect("session");
        session
            .bootstrap_portable(request_for_root(None, 10, "connection", DATABASE_ROOT))
            .expect_err(name);
        assert_eq!(api.call_count("bounded:query:"), 1, "{name}");
        assert_eq!(api.call_count("bounded:children:"), 0, "{name}");
        assert_eq!(
            api.call_count(&format!("bounded:page:{DATABASE_ROW}")),
            0,
            "{name}"
        );
        session
            .bootstrap_portable(request_for_root(None, 10, "connection", DATABASE_ROOT))
            .expect_err("failed session cannot publish a checkpoint");
        assert_eq!(api.call_count("bounded:query:"), 1, "{name}");
    }
}

#[test]
fn decoded_provider_pages_are_reserved_at_the_exact_retained_boundary() {
    fn block_attempt(cap: u64) -> (bool, Arc<FixtureApi>) {
        let block = BlockDto {
            id: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_string(),
            kind: "x".repeat(8_192),
            ..Default::default()
        };
        let api = Arc::new(FixtureApi::new(&[]).with_root_blocks(vec![block]));
        let connector = connector(api.clone());
        let session = connector
            .initial_hydration_session(CONNECTION_HASH, 10, limits_with_retained(cap))
            .expect("session");
        let succeeded = session
            .bootstrap_portable(request(None, 10, "connection"))
            .is_ok();
        (succeeded, api)
    }

    fn row_attempt(cap: u64) -> (bool, Arc<DatabaseFixtureApi>) {
        let api = Arc::new(
            DatabaseFixtureApi::new(Some(ParentDto {
                kind: "data_source_id".to_string(),
                data_source_id: Some(DATA_SOURCE.to_string()),
                ..Default::default()
            }))
            .with_row_padding(8_192),
        );
        let connector = connector_with_database_api(api.clone());
        let session = connector
            .initial_hydration_session(CONNECTION_HASH, 10, limits_with_retained(cap))
            .expect("session");
        let succeeded = session
            .bootstrap_portable(request_for_root(None, 10, "connection", DATABASE_ROOT))
            .is_ok();
        (succeeded, api)
    }

    fn minimum_success(mut attempt: impl FnMut(u64) -> bool) -> u64 {
        let mut low = 1_u64;
        let mut high = 100_000_u64;
        assert!(attempt(high));
        while low < high {
            let middle = low + (high - low) / 2;
            if attempt(middle) {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        low
    }

    let block_cap = minimum_success(|cap| block_attempt(cap).0);
    assert!(block_attempt(block_cap).0);
    let (succeeded, block_api) = block_attempt(block_cap - 1);
    assert!(!succeeded);
    assert_eq!(block_api.call_count("bounded:page:"), 1);
    assert_eq!(block_api.call_count("bounded:children:"), 1);

    let row_cap = minimum_success(|cap| row_attempt(cap).0);
    assert!(row_attempt(row_cap).0);
    let (succeeded, row_api) = row_attempt(row_cap - 1);
    assert!(!succeeded);
    assert_eq!(row_api.call_count("bounded:query:"), 1);
    assert_eq!(row_api.call_count("bounded:children:"), 0);
    assert_eq!(
        row_api.call_count(&format!("bounded:page:{DATABASE_ROW}")),
        0
    );
}

fn connector(api: Arc<FixtureApi>) -> NotionConnector {
    connector_with_api(api).with_root_ids([RemoteId::new(ROOT)])
}

fn connector_with_api(api: Arc<FixtureApi>) -> NotionConnector {
    NotionConnector::with_api(
        NotionConfig::default().with_token("credential-must-never-serialize"),
        api,
    )
}

fn connector_with_database_api(api: Arc<DatabaseFixtureApi>) -> NotionConnector {
    NotionConnector::with_api(
        NotionConfig::default().with_token("credential-must-never-serialize"),
        api,
    )
    .with_root_ids([RemoteId::new(DATABASE_ROOT)])
}

fn request(
    checkpoint: Option<locality_connector::PortableCheckpoint>,
    max_changes: u32,
    connection: &str,
) -> PortableBootstrapRequest {
    PortableBootstrapRequest {
        source_connection_id: SourceConnectionId::new(connection),
        scope: scope(),
        checkpoint,
        max_changes,
    }
}

fn request_for_root(
    checkpoint: Option<locality_connector::PortableCheckpoint>,
    max_changes: u32,
    connection: &str,
    root: &str,
) -> PortableBootstrapRequest {
    PortableBootstrapRequest {
        source_connection_id: SourceConnectionId::new(connection),
        scope: PortableSourceScope::explicit_roots([RemoteId::new(root)]),
        checkpoint,
        max_changes,
    }
}

fn scope() -> PortableSourceScope {
    PortableSourceScope::explicit_roots([RemoteId::new(ROOT)])
}

fn page(id: &str, parent: Option<&str>, version: &str) -> PageDto {
    PageDto {
        id: id.to_string(),
        parent: parent.map(|parent| ParentDto {
            kind: "page_id".to_string(),
            page_id: Some(parent.to_string()),
            ..Default::default()
        }),
        created_time: None,
        last_edited_time: Some(version.to_string()),
        archived: false,
        in_trash: false,
        properties: BTreeMap::new(),
    }
}

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
        max_retained_bytes: cap.saturating_mul(1_000),
    }
}

fn limits_with_retained(max_retained_bytes: u64) -> InitialHydrationLimits {
    InitialHydrationLimits {
        max_retained_bytes,
        ..limits(1_000_000)
    }
}

fn canonical(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character != '-')
        .flat_map(char::to_lowercase)
        .collect()
}
