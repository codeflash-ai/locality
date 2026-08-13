use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use locality_connector::{
    Connector, PORTABLE_SYNC_V2_MAX_PROVIDER_VERSION_BYTES, PortableBatchAuthority,
    PortableCheckpoint, PortableFetchReason, PortableFetchRequest, PortableSourceScope,
    PortableSyncHintV2, PortableSyncMode, PortableSyncRequestV2, dispatch_portable_sync_v2,
    portable_scope_root_remote_id,
};
use locality_core::model::{EntityKind, RemoteId};
use locality_core::portable::{LogicalPath, SourceConnectionId};
use locality_core::{LocalityError, LocalityResult};
use locality_notion::client::NotionApi;
use locality_notion::database::database_bundle_provider_version_token;
use locality_notion::dto::{
    BlockDto, BlockListDto, DataSourceDto, DataSourceSummaryDto, DatabaseDto, NotionDatabaseBundle,
    PageDto, PageListDto, ParentDto,
};
use locality_notion::{NotionConfig, NotionConnector};
use serde_json::json;
use sha2::{Digest, Sha256};

const ROOT_A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const ROOT_A_EXACT: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const ROOT_B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const ROOT_B_EXACT: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
const PAGE_1: &str = "11111111-1111-1111-1111-111111111111";
const PAGE_2: &str = "22222222-2222-2222-2222-222222222222";
const DATABASE: &str = "dddddddd-dddd-dddd-dddd-dddddddddddd";
const DATA_SOURCE: &str = "eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee";

#[derive(Debug, Default)]
struct RecordingApi {
    pages: BTreeMap<String, LocalityResult<PageDto>>,
    databases: BTreeMap<String, LocalityResult<DatabaseDto>>,
    data_sources: BTreeMap<String, LocalityResult<DataSourceDto>>,
    blocks: BTreeMap<String, LocalityResult<BlockDto>>,
    allow_legacy_block_inventory: bool,
    calls: Mutex<Vec<String>>,
}

impl RecordingApi {
    fn with_page(mut self, page: PageDto) -> Self {
        self.pages.insert(canonical(&page.id), Ok(page));
        self
    }

    fn with_page_error(mut self, id: &str, error: LocalityError) -> Self {
        self.pages.insert(canonical(id), Err(error));
        self
    }

    fn with_database(mut self, database: DatabaseDto) -> Self {
        self.databases.insert(canonical(&database.id), Ok(database));
        self
    }

    fn with_data_source(mut self, data_source: DataSourceDto) -> Self {
        self.data_sources
            .insert(canonical(&data_source.id), Ok(data_source));
        self
    }

    fn with_legacy_block_inventory(mut self) -> Self {
        self.allow_legacy_block_inventory = true;
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls").clone()
    }

    fn record(&self, call: String) {
        self.calls.lock().expect("calls").push(call);
    }
}

impl NotionApi for RecordingApi {
    fn retrieve_page(&self, page_id: &str) -> LocalityResult<PageDto> {
        self.record(format!("page:{page_id}"));
        self.pages
            .get(&canonical(page_id))
            .cloned()
            .unwrap_or_else(|| Err(LocalityError::RemoteNotFound(page_id.to_string())))
    }

    fn retrieve_database(&self, database_id: &str) -> LocalityResult<DatabaseDto> {
        self.record(format!("database:{database_id}"));
        self.databases
            .get(&canonical(database_id))
            .cloned()
            .unwrap_or_else(|| Err(LocalityError::RemoteNotFound(database_id.to_string())))
    }

    fn retrieve_data_source(&self, data_source_id: &str) -> LocalityResult<DataSourceDto> {
        self.record(format!("data_source:{data_source_id}"));
        self.data_sources
            .get(&canonical(data_source_id))
            .cloned()
            .unwrap_or_else(|| Err(LocalityError::RemoteNotFound(data_source_id.to_string())))
    }

    fn retrieve_block(&self, block_id: &str) -> LocalityResult<BlockDto> {
        self.record(format!("block:{block_id}"));
        self.blocks
            .get(&canonical(block_id))
            .cloned()
            .unwrap_or_else(|| Err(LocalityError::RemoteNotFound(block_id.to_string())))
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        _start_cursor: Option<&str>,
    ) -> LocalityResult<BlockListDto> {
        if self.allow_legacy_block_inventory {
            self.record(format!("block_children:{block_id}"));
            return Ok(BlockListDto::default());
        }
        panic!("hints-only sync must not enumerate block children for {block_id}")
    }

    fn search_pages(&self, _start_cursor: Option<&str>) -> LocalityResult<PageListDto> {
        panic!("hints-only sync must not search pages")
    }

    fn search_databases(
        &self,
        _start_cursor: Option<&str>,
    ) -> LocalityResult<locality_notion::dto::DatabaseListDto> {
        panic!("hints-only sync must not search databases")
    }

    fn query_data_source(
        &self,
        data_source_id: &str,
        _start_cursor: Option<&str>,
    ) -> LocalityResult<PageListDto> {
        panic!("hints-only sync must not query data source {data_source_id}")
    }

    fn update_block(&self, _block_id: &str, _body: serde_json::Value) -> LocalityResult<BlockDto> {
        panic!("unexpected mutation")
    }

    fn append_block_children(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> LocalityResult<BlockListDto> {
        panic!("unexpected mutation")
    }

    fn delete_block(&self, _block_id: &str) -> LocalityResult<BlockDto> {
        panic!("unexpected mutation")
    }
}

#[test]
fn hints_only_preserves_exact_root_and_upserts_page_with_same_timestamp() {
    let timestamp = "2026-08-13T01:02:03.000Z";
    let api =
        Arc::new(RecordingApi::default().with_page(page(PAGE_1, page_parent(ROOT_A), timestamp)));
    let connector = connector(api.clone(), [ROOT_A]);
    let request = request(
        [ROOT_A_EXACT],
        terminal_v1_checkpoint(ROOT_A_EXACT),
        vec![hint(
            PAGE_1,
            Some(timestamp),
            "Prior Exact/Child/page.md",
            EntityKind::Page,
            ROOT_A_EXACT,
        )],
        8,
    );

    let batch = dispatch_portable_sync_v2(&connector, request).expect("hints-only batch");
    assert_eq!(batch.authority, PortableBatchAuthority::Incremental);
    assert!(batch.covered_root_remote_ids.is_empty());
    assert_eq!(batch.changes.len(), 1);
    let change = &batch.changes[0];
    assert!(!change.source_object.deleted);
    assert!(change.requires_fetch);
    assert_eq!(
        change.logical_path.as_ref().map(LogicalPath::as_str),
        Some("Prior Exact/Child/page.md")
    );
    assert_eq!(
        portable_scope_root_remote_id(&change.source_object).expect("root edge"),
        Some(&RemoteId::new(ROOT_A_EXACT))
    );
    assert_eq!(batch.next_checkpoint.format_version, 3);
    let checkpoint: serde_json::Value =
        serde_json::from_str(&batch.next_checkpoint.opaque).expect("v3 checkpoint");
    assert_eq!(checkpoint["exact_root_remote_ids"], json!([ROOT_A_EXACT]));
    assert_eq!(
        checkpoint["canonical_root_remote_ids"],
        json!([canonical(ROOT_A)])
    );
    assert_eq!(api.calls(), vec![format!("page:{PAGE_1}")]);
}

#[test]
fn reconcile_scope_retains_legacy_incremental_authority_without_coverage() {
    let api = Arc::new(
        RecordingApi::default()
            .with_page(page(ROOT_A, workspace_parent(), "root-v1"))
            .with_legacy_block_inventory(),
    );
    let connector = NotionConnector::with_api(
        NotionConfig::default().with_root_page_id(RemoteId::new(ROOT_A)),
        api,
    );
    let mut request = request([ROOT_A], terminal_v1_checkpoint(ROOT_A), Vec::new(), 8);
    request.mode = PortableSyncMode::ReconcileScope;

    let batch = dispatch_portable_sync_v2(&connector, request).expect("compatibility reconcile");
    assert_eq!(batch.authority, PortableBatchAuthority::Incremental);
    assert!(batch.covered_root_remote_ids.is_empty());
    assert_eq!(batch.changes.len(), 1);
}

#[test]
fn data_source_and_database_hints_emit_one_composite_database_change() {
    let database = DatabaseDto {
        id: DATABASE.to_string(),
        parent: Some(page_parent(ROOT_A)),
        last_edited_time: Some("db-v1".to_string()),
        data_sources: vec![DataSourceSummaryDto {
            id: DATA_SOURCE.to_string(),
            name: Some("Tasks".to_string()),
        }],
        ..Default::default()
    };
    let data_source = DataSourceDto {
        id: DATA_SOURCE.to_string(),
        parent: Some(database_parent(DATABASE)),
        last_edited_time: Some("ds-v2".to_string()),
        ..Default::default()
    };
    let provider_version = database_bundle_provider_version_token(&NotionDatabaseBundle {
        database: database.clone(),
        data_sources: vec![data_source.clone()],
    })
    .expect("database provider token");
    let api = Arc::new(
        RecordingApi::default()
            .with_database(database)
            .with_data_source(data_source),
    );
    let connector = connector(api.clone(), [ROOT_A]);
    let hints = vec![
        hint(
            DATABASE,
            Some(&provider_version),
            "Root/Tasks/_schema.yaml",
            EntityKind::Database,
            ROOT_A,
        ),
        hint(
            DATA_SOURCE,
            Some("ds-v1"),
            "Root/Tasks/_schema.yaml",
            EntityKind::Unknown("data_source".to_string()),
            ROOT_A,
        ),
    ];

    let batch = dispatch_portable_sync_v2(
        &connector,
        request([ROOT_A], terminal_v1_checkpoint(ROOT_A), hints, 8),
    )
    .expect("deduplicated database batch");
    assert_eq!(batch.changes.len(), 1);
    let change = &batch.changes[0];
    assert_eq!(change.source_object.remote_id, RemoteId::new(DATABASE));
    assert_eq!(change.source_object.kind, EntityKind::Database);
    let emitted_provider_version = change
        .source_object
        .opaque_version
        .as_ref()
        .expect("composite provider version");
    assert_eq!(emitted_provider_version, &provider_version);
    assert!(emitted_provider_version.len() <= PORTABLE_SYNC_V2_MAX_PROVIDER_VERSION_BYTES);
    assert_eq!(
        api.calls(),
        vec![
            format!("database:{DATABASE}"),
            format!("data_source:{DATA_SOURCE}")
        ]
    );
}

#[test]
fn data_source_first_uses_later_database_hint_when_its_path_is_missing() {
    let database = DatabaseDto {
        id: DATABASE.to_string(),
        parent: Some(page_parent(ROOT_A)),
        last_edited_time: Some("db-v1".to_string()),
        data_sources: vec![DataSourceSummaryDto {
            id: DATA_SOURCE.to_string(),
            name: Some("Tasks".to_string()),
        }],
        ..Default::default()
    };
    let data_source = DataSourceDto {
        id: DATA_SOURCE.to_string(),
        parent: Some(database_parent(DATABASE)),
        last_edited_time: Some("ds-v1".to_string()),
        ..Default::default()
    };
    let provider_version = database_bundle_provider_version_token(&NotionDatabaseBundle {
        database: database.clone(),
        data_sources: vec![data_source.clone()],
    })
    .expect("database provider token");
    let api = Arc::new(
        RecordingApi::default()
            .with_database(database)
            .with_data_source(data_source),
    );
    let connector = connector(api.clone(), [ROOT_A]);
    let mut data_source_hint = hint(
        DATA_SOURCE,
        Some("data-source-prior"),
        "Unused/Schema/_schema.yaml",
        EntityKind::Unknown("data_source".to_string()),
        ROOT_A,
    );
    data_source_hint.logical_path = None;
    let batch = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A],
            terminal_v1_checkpoint(ROOT_A),
            vec![
                data_source_hint,
                hint(
                    DATABASE,
                    Some(&provider_version),
                    "Root/Canonical DB/_schema.yaml",
                    EntityKind::Database,
                    ROOT_A,
                ),
            ],
            8,
        ),
    )
    .expect("data-source-first coalescing");
    assert_eq!(batch.changes.len(), 1);
    assert_eq!(
        batch.changes[0]
            .logical_path
            .as_ref()
            .map(LogicalPath::as_str),
        Some("Root/Canonical DB/_schema.yaml")
    );
    assert_eq!(
        batch.changes[0].source_object.opaque_version.as_deref(),
        Some(provider_version.as_str())
    );
    assert_eq!(
        api.calls(),
        vec![
            format!("data_source:{DATA_SOURCE}"),
            format!("database:{DATABASE}")
        ]
    );
}

#[test]
fn data_source_first_archived_database_uses_later_database_path_and_prior_root() {
    let mut database = DatabaseDto {
        id: DATABASE.to_string(),
        parent: Some(page_parent(ROOT_A)),
        last_edited_time: Some("archived-v2".to_string()),
        ..Default::default()
    };
    database.archived = true;
    let data_source = DataSourceDto {
        id: DATA_SOURCE.to_string(),
        parent: Some(database_parent(DATABASE)),
        ..Default::default()
    };
    let api = Arc::new(
        RecordingApi::default()
            .with_database(database)
            .with_data_source(data_source),
    );
    let connector = connector(api, [ROOT_A, ROOT_B]);
    let batch = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A_EXACT, ROOT_B_EXACT],
            terminal_v2_checkpoint([ROOT_A_EXACT, ROOT_B_EXACT]),
            vec![
                hint(
                    DATA_SOURCE,
                    Some("data-source-prior"),
                    "Wrong Root/Wrong DB/_schema.yaml",
                    EntityKind::Unknown("data_source".to_string()),
                    ROOT_B_EXACT,
                ),
                hint(
                    DATABASE,
                    Some("database-prior"),
                    "Right Root/Right DB/_schema.yaml",
                    EntityKind::Database,
                    ROOT_A_EXACT,
                ),
            ],
            8,
        ),
    )
    .expect("archived database coalescing");
    assert_eq!(batch.changes.len(), 1);
    let change = &batch.changes[0];
    assert!(change.source_object.deleted);
    assert_eq!(
        change.logical_path.as_ref().map(LogicalPath::as_str),
        Some("Right Root/Right DB/_schema.yaml")
    );
    assert_eq!(
        portable_scope_root_remote_id(&change.source_object).expect("database prior root"),
        Some(&RemoteId::new(ROOT_A_EXACT))
    );
}

#[test]
fn maximum_database_fanout_roundtrips_a_publicly_valid_provider_token() {
    let data_sources = (0..100)
        .map(|index| DataSourceDto {
            id: format!("{:032x}", index + 0x100),
            parent: Some(database_parent(DATABASE)),
            last_edited_time: Some(format!("ds-version-{index}")),
            ..Default::default()
        })
        .collect::<Vec<_>>();
    let database = DatabaseDto {
        id: DATABASE.to_string(),
        parent: Some(page_parent(ROOT_A)),
        last_edited_time: Some("db-version".to_string()),
        data_sources: data_sources
            .iter()
            .map(|data_source| DataSourceSummaryDto {
                id: data_source.id.clone(),
                name: None,
            })
            .collect(),
        ..Default::default()
    };
    let expected_token = database_bundle_provider_version_token(&NotionDatabaseBundle {
        database: database.clone(),
        data_sources: data_sources.clone(),
    })
    .expect("maximum-fanout provider token");
    assert!(expected_token.len() <= PORTABLE_SYNC_V2_MAX_PROVIDER_VERSION_BYTES);

    let mut recording = RecordingApi::default().with_database(database);
    for data_source in data_sources {
        recording = recording.with_data_source(data_source);
    }
    let api = Arc::new(recording);
    let connector = connector(api, [ROOT_A]);
    let first_request = request(
        [ROOT_A],
        terminal_v1_checkpoint(ROOT_A),
        vec![hint(
            DATABASE,
            None,
            "Root/DB/_schema.yaml",
            EntityKind::Database,
            ROOT_A,
        )],
        8,
    );
    let scope = first_request.scope.clone();
    let first = dispatch_portable_sync_v2(&connector, first_request).expect("maximum-fanout sync");
    first
        .validate_for_request(&scope, 8)
        .expect("public response validation");
    assert_eq!(first.changes.len(), 1);
    assert_eq!(
        first.changes[0].source_object.opaque_version.as_deref(),
        Some(expected_token.as_str())
    );

    let fetched = connector
        .fetch_portable(PortableFetchRequest {
            source_connection_id: SourceConnectionId::new("source-notion"),
            remote_id: RemoteId::new(DATABASE),
            reason: PortableFetchReason::Synchronization,
        })
        .expect("maximum-fanout fetch");
    assert_eq!(
        fetched.provider_version.as_deref(),
        Some(expected_token.as_str())
    );

    let second = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A],
            first.next_checkpoint,
            vec![hint(
                DATABASE,
                fetched.provider_version.as_deref(),
                "Root/DB/_schema.yaml",
                EntityKind::Database,
                ROOT_A,
            )],
            8,
        ),
    )
    .expect("provider-token continuation round trip");
    second
        .validate_for_request(&scope, 8)
        .expect("round-trip public response validation");
    assert!(second.changes.is_empty());
}

#[test]
fn direct_archives_and_proven_outside_moves_are_tombstones() {
    let mut archived = page(PAGE_1, malformed_parent(), "v2");
    archived.archived = true;
    let moved_outside = page(PAGE_2, workspace_parent(), "v3");
    let api = Arc::new(
        RecordingApi::default()
            .with_page(archived)
            .with_page(moved_outside),
    );
    let connector = connector(api, [ROOT_A]);
    let batch = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A],
            terminal_v1_checkpoint(ROOT_A),
            vec![
                hint(
                    PAGE_1,
                    Some("v1"),
                    "Root/Archived/page.md",
                    EntityKind::Page,
                    ROOT_A,
                ),
                hint(
                    PAGE_2,
                    Some("v2"),
                    "Root/Moved/page.md",
                    EntityKind::Page,
                    ROOT_A,
                ),
            ],
            8,
        ),
    )
    .expect("tombstone batch");
    assert_eq!(batch.changes.len(), 2);
    assert!(
        batch
            .changes
            .iter()
            .all(|change| { change.source_object.deleted && !change.requires_fetch })
    );
}

#[test]
fn move_between_requested_roots_is_one_rehomed_upsert() {
    let api = Arc::new(RecordingApi::default().with_page(page(PAGE_1, page_parent(ROOT_B), "v2")));
    let connector = connector(api, [ROOT_A, ROOT_B]);
    let batch = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A_EXACT, ROOT_B_EXACT],
            terminal_v2_checkpoint([ROOT_A_EXACT, ROOT_B_EXACT]),
            vec![hint(
                PAGE_1,
                Some("v1"),
                "Old Root/Child/page.md",
                EntityKind::Page,
                ROOT_A_EXACT,
            )],
            8,
        ),
    )
    .expect("rehome batch");
    assert_eq!(batch.changes.len(), 1);
    let change = &batch.changes[0];
    assert!(!change.source_object.deleted);
    assert_eq!(
        portable_scope_root_remote_id(&change.source_object).expect("root edge"),
        Some(&RemoteId::new(ROOT_B_EXACT))
    );
    assert_eq!(
        change.logical_path.as_ref().map(LogicalPath::as_str),
        Some("Old Root/Child/page.md")
    );
}

#[test]
fn database_move_between_roots_upserts_even_when_provider_token_is_unchanged() {
    let database = DatabaseDto {
        id: DATABASE.to_string(),
        parent: Some(page_parent(ROOT_B)),
        last_edited_time: Some("same-version".to_string()),
        ..Default::default()
    };
    let provider_version = database_bundle_provider_version_token(&NotionDatabaseBundle {
        database: database.clone(),
        data_sources: Vec::new(),
    })
    .expect("database provider token");
    let api = Arc::new(RecordingApi::default().with_database(database));
    let connector = connector(api, [ROOT_A, ROOT_B]);
    let batch = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A_EXACT, ROOT_B_EXACT],
            terminal_v2_checkpoint([ROOT_A_EXACT, ROOT_B_EXACT]),
            vec![hint(
                DATABASE,
                Some(&provider_version),
                "Old Root/DB/_schema.yaml",
                EntityKind::Database,
                ROOT_A_EXACT,
            )],
            8,
        ),
    )
    .expect("database rehome batch");
    assert_eq!(batch.changes.len(), 1);
    let change = &batch.changes[0];
    assert!(!change.source_object.deleted);
    assert_eq!(change.source_object.opaque_version, Some(provider_version));
    assert_eq!(
        portable_scope_root_remote_id(&change.source_object).expect("root edge"),
        Some(&RemoteId::new(ROOT_B_EXACT))
    );
}

#[test]
fn not_found_rate_limit_and_malformed_ancestry_fail_without_tombstones() {
    for error in [
        LocalityError::RemoteNotFound("hidden".to_string()),
        LocalityError::RateLimited {
            provider: "notion".to_string(),
            retry_after: Duration::from_secs(7),
            message: "cooldown".to_string(),
        },
    ] {
        let api = Arc::new(RecordingApi::default().with_page_error(PAGE_1, error.clone()));
        let connector = connector(api, [ROOT_A]);
        let result = dispatch_portable_sync_v2(
            &connector,
            request(
                [ROOT_A],
                terminal_v1_checkpoint(ROOT_A),
                vec![hint(
                    PAGE_1,
                    Some("v1"),
                    "Root/Page/page.md",
                    EntityKind::Page,
                    ROOT_A,
                )],
                8,
            ),
        );
        assert_eq!(result.expect_err("metadata error must propagate"), error);
    }

    let api = Arc::new(RecordingApi::default().with_page(page(PAGE_1, malformed_parent(), "v2")));
    let connector = connector(api, [ROOT_A]);
    let error = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A],
            terminal_v1_checkpoint(ROOT_A),
            vec![hint(
                PAGE_1,
                Some("v1"),
                "Root/Page/page.md",
                EntityKind::Page,
                ROOT_A,
            )],
            8,
        ),
    )
    .expect_err("malformed parent must fail");
    assert!(error.to_string().contains("parent"));
}

#[test]
fn contradictory_workspace_parent_metadata_cannot_prove_an_outside_move() {
    for workspace in [None, Some(false)] {
        let parent = ParentDto {
            kind: "workspace".to_string(),
            workspace,
            ..Default::default()
        };
        let api = Arc::new(RecordingApi::default().with_page(page(PAGE_1, parent, "v2")));
        let connector = connector(api, [ROOT_A]);
        let error = dispatch_portable_sync_v2(
            &connector,
            request(
                [ROOT_A],
                terminal_v1_checkpoint(ROOT_A),
                vec![hint(
                    PAGE_1,
                    Some("v1"),
                    "Root/Page/page.md",
                    EntityKind::Page,
                    ROOT_A,
                )],
                8,
            ),
        )
        .expect_err("contradictory workspace parent must not become a tombstone");
        assert!(error.to_string().contains("malformed workspace parent"));
    }
}

#[test]
fn continuation_binds_semantic_hints_roots_and_progress() {
    let api = Arc::new(
        RecordingApi::default()
            .with_page(page(PAGE_1, page_parent(ROOT_A), "v2"))
            .with_page(page(PAGE_2, page_parent(ROOT_A), "v2")),
    );
    let connector = connector(api.clone(), [ROOT_A]);
    let hints = vec![
        hint(
            PAGE_1,
            Some("v1"),
            "Root/One/page.md",
            EntityKind::Page,
            ROOT_A_EXACT,
        ),
        hint(
            PAGE_2,
            Some("v1"),
            "Root/Two/page.md",
            EntityKind::Page,
            ROOT_A_EXACT,
        ),
    ];
    let first = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A_EXACT],
            terminal_v1_checkpoint(ROOT_A_EXACT),
            hints.clone(),
            1,
        ),
    )
    .expect("first page");
    assert_eq!(first.changes.len(), 1);
    assert!(!first.completeness.is_complete());

    let second = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A_EXACT],
            first.next_checkpoint.clone(),
            hints.clone(),
            1,
        ),
    )
    .expect("continuation");
    assert_eq!(second.changes.len(), 1);
    assert!(second.completeness.is_complete());

    let mut changed_hints = hints.clone();
    changed_hints[1].provider_version = Some("different".to_string());
    let error = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A_EXACT],
            first.next_checkpoint.clone(),
            changed_hints,
            1,
        ),
    )
    .expect_err("changed hints must not resume");
    assert!(error.to_string().contains("semantic hints"));

    let changed_root_spelling = ROOT_A.to_string();
    let changed_root_hints = hints
        .iter()
        .cloned()
        .map(|mut hint| {
            hint.owning_root_remote_id = Some(RemoteId::new(changed_root_spelling.clone()));
            hint
        })
        .collect();
    let error = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A],
            first.next_checkpoint.clone(),
            changed_root_hints,
            1,
        ),
    )
    .expect_err("changed exact root spelling must not resume");
    assert!(error.to_string().contains("exact root set"));

    let mut replay = first.next_checkpoint;
    let mut replay_json: serde_json::Value =
        serde_json::from_str(&replay.opaque).expect("checkpoint JSON");
    replay_json["next_index"] = json!(0);
    replay.opaque = serde_json::to_string(&replay_json).expect("checkpoint JSON");
    let error = dispatch_portable_sync_v2(&connector, request([ROOT_A_EXACT], replay, hints, 1))
        .expect_err("zero-progress continuation must fail");
    assert!(error.to_string().contains("replay, cycle"));
    assert_eq!(api.calls().len(), 2);
}

#[test]
fn continuation_rejects_changed_mode_and_future_component() {
    let api = Arc::new(
        RecordingApi::default()
            .with_page(page(PAGE_1, page_parent(ROOT_A), "v2"))
            .with_page(page(PAGE_2, page_parent(ROOT_A), "v2")),
    );
    let connector = connector(api.clone(), [ROOT_A]);
    let hints = vec![
        hint(
            PAGE_1,
            Some("v1"),
            "Root/One/page.md",
            EntityKind::Page,
            ROOT_A,
        ),
        hint(
            PAGE_2,
            Some("v1"),
            "Root/Two/page.md",
            EntityKind::Page,
            ROOT_A,
        ),
    ];
    let first = dispatch_portable_sync_v2(
        &connector,
        request([ROOT_A], terminal_v1_checkpoint(ROOT_A), hints.clone(), 1),
    )
    .expect("first page");

    for (field, value, expected) in [
        ("mode", json!("reconcile_scope"), "mode"),
        ("component_version", json!(4), "requires an update"),
    ] {
        let mut checkpoint = first.next_checkpoint.clone();
        let mut checkpoint_json: serde_json::Value =
            serde_json::from_str(&checkpoint.opaque).expect("checkpoint JSON");
        checkpoint_json[field] = value;
        checkpoint.opaque = serde_json::to_string(&checkpoint_json).expect("checkpoint JSON");
        let error =
            dispatch_portable_sync_v2(&connector, request([ROOT_A], checkpoint, hints.clone(), 1))
                .expect_err("changed checkpoint binding");
        assert!(error.to_string().contains(expected), "{error}");
    }
    assert_eq!(api.calls(), vec![format!("page:{PAGE_1}")]);
}

#[test]
fn legacy_nonterminal_future_and_oversized_checkpoints_fail_closed() {
    let api = Arc::new(RecordingApi::default().with_page(page(PAGE_1, page_parent(ROOT_A), "v2")));
    let connector = connector(api.clone(), [ROOT_A]);
    let hints = vec![hint(
        PAGE_1,
        Some("v1"),
        "Root/Page/page.md",
        EntityKind::Page,
        ROOT_A,
    )];

    let mut nonterminal = terminal_v1_checkpoint(ROOT_A);
    let mut json: serde_json::Value =
        serde_json::from_str(&nonterminal.opaque).expect("legacy JSON");
    json["complete"] = json!(false);
    nonterminal.opaque = serde_json::to_string(&json).expect("legacy JSON");
    assert!(
        dispatch_portable_sync_v2(&connector, request([ROOT_A], nonterminal, hints.clone(), 8))
            .expect_err("nonterminal legacy checkpoint")
            .to_string()
            .contains("only a terminal")
    );

    let future = PortableCheckpoint {
        format_version: 4,
        opaque: "{}".to_string(),
    };
    assert!(
        dispatch_portable_sync_v2(&connector, request([ROOT_A], future, hints.clone(), 8))
            .expect_err("future checkpoint")
            .to_string()
            .contains("requires an update")
    );

    let oversized = PortableCheckpoint {
        format_version: 3,
        opaque: "x".repeat(16 * 1024 + 1),
    };
    assert!(
        dispatch_portable_sync_v2(&connector, request([ROOT_A], oversized, hints, 8))
            .expect_err("connector checkpoint ceiling")
            .to_string()
            .contains("maximum is 16384")
    );
    assert!(api.calls().is_empty());
}

#[test]
fn connector_specific_hint_and_database_bounds_fail_before_unbounded_work() {
    let hints = (0..33)
        .map(|index| {
            hint(
                &format!("{index:032x}"),
                None,
                &format!("Root/{index}/page.md"),
                EntityKind::Page,
                ROOT_A,
            )
        })
        .collect();
    let api = Arc::new(RecordingApi::default());
    let hint_connector = connector(api.clone(), [ROOT_A]);
    let error = dispatch_portable_sync_v2(
        &hint_connector,
        request([ROOT_A], terminal_v1_checkpoint(ROOT_A), hints, 100),
    )
    .expect_err("Notion hint ceiling");
    assert!(error.to_string().contains("maximum is 32"));
    assert!(api.calls().is_empty());

    let summaries = (0..101)
        .map(|index| DataSourceSummaryDto {
            id: format!("{index:032x}"),
            name: None,
        })
        .collect();
    let database = DatabaseDto {
        id: DATABASE.to_string(),
        parent: Some(page_parent(ROOT_A)),
        data_sources: summaries,
        ..Default::default()
    };
    let api = Arc::new(RecordingApi::default().with_database(database));
    let depth_connector = connector(api.clone(), [ROOT_A]);
    let error = dispatch_portable_sync_v2(
        &depth_connector,
        request(
            [ROOT_A],
            terminal_v1_checkpoint(ROOT_A),
            vec![hint(
                DATABASE,
                None,
                "Root/DB/_schema.yaml",
                EntityKind::Database,
                ROOT_A,
            )],
            8,
        ),
    )
    .expect_err("data-source ceiling");
    assert!(error.to_string().contains("limit of 100 data sources"));
    assert_eq!(api.calls(), vec![format!("database:{DATABASE}")]);
}

#[test]
fn ancestry_duplicate_summary_and_canonical_hint_bounds_fail_closed() {
    let mut api = RecordingApi::default();
    let ancestor_ids = (0..32)
        .map(|index| format!("f{index:031x}"))
        .collect::<Vec<_>>();
    api = api.with_page(page(PAGE_1, page_parent(&ancestor_ids[0]), "v2"));
    for (index, ancestor_id) in ancestor_ids.iter().enumerate() {
        let parent = ancestor_ids
            .get(index + 1)
            .map(|next| page_parent(next))
            .unwrap_or_else(workspace_parent);
        api = api.with_page(page(ancestor_id, parent, "ancestor"));
    }
    let api = Arc::new(api);
    let duplicate_connector = connector(api.clone(), [ROOT_A]);
    let error = dispatch_portable_sync_v2(
        &duplicate_connector,
        request(
            [ROOT_A],
            terminal_v1_checkpoint(ROOT_A),
            vec![hint(
                PAGE_1,
                Some("v1"),
                "Root/Page/page.md",
                EntityKind::Page,
                ROOT_A,
            )],
            8,
        ),
    )
    .expect_err("ancestry depth ceiling");
    assert!(error.to_string().contains("depth limit of 32"));
    assert_eq!(api.calls().len(), 33);

    let duplicated_summaries = (0..34)
        .map(|_| DataSourceSummaryDto {
            id: DATA_SOURCE.to_string(),
            name: Some("Tasks".to_string()),
        })
        .collect();
    let api = Arc::new(RecordingApi::default().with_database(DatabaseDto {
        id: DATABASE.to_string(),
        parent: Some(page_parent(ROOT_A)),
        data_sources: duplicated_summaries,
        ..Default::default()
    }));
    let hint_connector = connector(api.clone(), [ROOT_A]);
    let error = dispatch_portable_sync_v2(
        &hint_connector,
        request(
            [ROOT_A],
            terminal_v1_checkpoint(ROOT_A),
            vec![hint(
                DATABASE,
                None,
                "Root/DB/_schema.yaml",
                EntityKind::Database,
                ROOT_A,
            )],
            8,
        ),
    )
    .expect_err("duplicate summary ceiling");
    assert!(error.to_string().contains("32 equivalent duplicate"));
    assert_eq!(api.calls(), vec![format!("database:{DATABASE}")]);

    let api = Arc::new(RecordingApi::default());
    let connector = connector(api.clone(), [ROOT_A]);
    let error = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A],
            terminal_v1_checkpoint(ROOT_A),
            vec![
                hint(PAGE_1, None, "Root/A/page.md", EntityKind::Page, ROOT_A),
                hint(
                    &canonical(PAGE_1),
                    None,
                    "Root/B/page.md",
                    EntityKind::Page,
                    ROOT_A,
                ),
            ],
            8,
        ),
    )
    .expect_err("canonical duplicate hint IDs");
    assert!(error.to_string().contains("canonically duplicate"));
    assert!(api.calls().is_empty());
}

#[test]
fn data_source_disappearance_is_an_error_not_a_database_tombstone() {
    let api = Arc::new(RecordingApi::default());
    let connector = connector(api.clone(), [ROOT_A]);
    let error = dispatch_portable_sync_v2(
        &connector,
        request(
            [ROOT_A],
            terminal_v1_checkpoint(ROOT_A),
            vec![hint(
                DATA_SOURCE,
                Some("old"),
                "Root/DB/_schema.yaml",
                EntityKind::Unknown("data_source".to_string()),
                ROOT_A,
            )],
            8,
        ),
    )
    .expect_err("missing data source");
    assert!(matches!(error, LocalityError::RemoteNotFound(_)));
    assert_eq!(api.calls(), vec![format!("data_source:{DATA_SOURCE}")]);
}

fn connector(
    api: Arc<RecordingApi>,
    roots: impl IntoIterator<Item = &'static str>,
) -> NotionConnector {
    NotionConnector::with_api(NotionConfig::default(), api).with_root_ids(
        roots
            .into_iter()
            .map(|root| RemoteId::new(root.to_string())),
    )
}

fn request(
    roots: impl IntoIterator<Item = impl Into<String>>,
    checkpoint: PortableCheckpoint,
    hints: Vec<PortableSyncHintV2>,
    max_changes: u32,
) -> PortableSyncRequestV2 {
    PortableSyncRequestV2 {
        source_connection_id: SourceConnectionId::new("source-notion"),
        scope: PortableSourceScope::explicit_roots(
            roots.into_iter().map(|root| RemoteId::new(root.into())),
        ),
        checkpoint,
        mode: PortableSyncMode::HintsOnly,
        hints,
        max_changes,
    }
}

fn hint(
    remote_id: &str,
    provider_version: Option<&str>,
    logical_path: &str,
    source_kind: EntityKind,
    owning_root: &str,
) -> PortableSyncHintV2 {
    PortableSyncHintV2 {
        remote_id: RemoteId::new(remote_id),
        provider_version: provider_version.map(str::to_string),
        logical_path: Some(LogicalPath::new(logical_path).expect("logical path")),
        source_kind: Some(source_kind),
        owning_root_remote_id: Some(RemoteId::new(owning_root)),
    }
}

fn terminal_v1_checkpoint(root: &str) -> PortableCheckpoint {
    PortableCheckpoint {
        format_version: 1,
        opaque: json!({
            "operation": "bootstrap",
            "root_remote_id": root,
            "inventory_sha256": "legacy-terminal",
            "offset": 1,
            "complete": true
        })
        .to_string(),
    }
}

fn terminal_v2_checkpoint(
    roots: impl IntoIterator<Item = impl Into<String>>,
) -> PortableCheckpoint {
    let mut canonical_roots = roots
        .into_iter()
        .map(|root| canonical(&root.into()))
        .collect::<Vec<_>>();
    canonical_roots.sort();
    PortableCheckpoint {
        format_version: 2,
        opaque: json!({
            "component_version": 2,
            "operation": "bootstrap",
            "root_set_sha256": canonical_root_identity(&canonical_roots),
            "root_remote_ids": canonical_roots,
            "inventory_sha256": "root-set-terminal",
            "offset": 2,
            "complete": true
        })
        .to_string(),
    }
}

fn canonical_root_identity(roots: &[String]) -> String {
    let mut hasher = Sha256::new();
    for root in roots {
        hasher.update((root.len() as u64).to_be_bytes());
        hasher.update(root.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn canonical(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

fn page(id: &str, parent: ParentDto, version: &str) -> PageDto {
    PageDto {
        id: id.to_string(),
        parent: Some(parent),
        created_time: None,
        last_edited_time: Some(version.to_string()),
        archived: false,
        in_trash: false,
        properties: BTreeMap::new(),
    }
}

fn page_parent(id: &str) -> ParentDto {
    ParentDto {
        kind: "page_id".to_string(),
        page_id: Some(id.to_string()),
        ..Default::default()
    }
}

fn database_parent(id: &str) -> ParentDto {
    ParentDto {
        kind: "database_id".to_string(),
        database_id: Some(id.to_string()),
        ..Default::default()
    }
}

fn workspace_parent() -> ParentDto {
    ParentDto {
        kind: "workspace".to_string(),
        workspace: Some(true),
        ..Default::default()
    }
}

fn malformed_parent() -> ParentDto {
    ParentDto {
        kind: "page_id".to_string(),
        page_id: Some(ROOT_A.to_string()),
        database_id: Some(DATABASE.to_string()),
        ..Default::default()
    }
}
