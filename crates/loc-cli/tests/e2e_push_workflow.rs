use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use loc_cli::connect::{
    BrokerOAuthConnectOptions, DEFAULT_GOOGLE_DOCS_OAUTH_PROFILE_ID,
    DEFAULT_NOTION_OAUTH_PROFILE_ID, DEFAULT_NOTION_PROFILE_ID,
    GoogleDocsBrokerOAuthConnectOptions, GoogleDocsOAuthBrokerExchange, NotionOAuthBrokerExchange,
    run_connect_google_docs_broker_oauth, run_connect_notion_broker_oauth, run_connection_show,
    run_connections, run_profiles,
};
use loc_cli::diff::{PushOperationOutput, run_diff, run_diff_with_state_root};
use loc_cli::history::{LogOptions, UndoOperationOutput, run_log, run_undo, run_undo_with_applier};
use loc_cli::inspect::{InspectOptions, run_inspect};
use loc_cli::mount::{GuidanceFileAction, MountOptions, run_mount};
use loc_cli::pull::{run_pull, run_pull_with_state_root};
use loc_cli::push::{PushOptions, run_push_with_daemon, run_push_with_daemon_at_state_root};
use loc_cli::restore::{RestoreOptions, run_restore};
use loc_cli::search::{SearchOptions, notion_id_from_url, run_search};
use loc_cli::status::{StatusOptions, StatusSyncState, run_status};
use locality_connector::oauth_broker::{OAuthBrokerCodeExchange, OAuthBrokerToken};
use locality_connector::{
    ApplyPlanRequest, ApplyPlanResult, ApplyUndoRequest, ApplyUndoResult, Connector,
    ConnectorCapabilities, ConnectorKind, ConnectorUndoApplier, EnumerateRequest, FetchRequest,
    ListChildrenRequest, ListChildrenResult, NativeEntity, ObserveRequest, ParsedEntity,
};
use locality_core::canonical::render_canonical_markdown;
use locality_core::conflict::{
    CONFLICT_LOCAL_MARKER, CONFLICT_REMOTE_MARKER, CONFLICT_SEPARATOR_MARKER,
    has_unresolved_conflict_markers,
};
use locality_core::explain::{RemoteChangeAction, RemoteChangeState};
use locality_core::freshness::{
    ChangeHintKind, FreshnessTier, RemoteObservation, RemoteVersion, SyncJob, SyncJobKind,
};
use locality_core::hydration::{HydrationPolicy, HydrationReason, HydrationRequest};
use locality_core::journal::{JournalEntry, JournalPreimage, JournalStatus, PushId};
use locality_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use locality_core::planner::{PushOperation, PushPlan};
use locality_core::shadow::{MarkdownBlockKind, ShadowDocument};
use locality_google_docs::client::{GoogleDocsApi, GoogleDriveApi};
use locality_google_docs::docs_dto::{BatchUpdateDocumentRequest, DocsRequest, GoogleDocument};
use locality_google_docs::drive_dto::{
    DRIVE_FOLDER_MIME_TYPE, DRIVE_GOOGLE_DOC_MIME_TYPE, DriveCreateFileRequest, DriveFile,
    DriveFileList, DriveUpdateFileRequest,
};
use locality_google_docs::{
    GOOGLE_DOCS_OAUTH_SCOPES, GoogleDocsConfig, GoogleDocsConnector, StoredGoogleDocsCredential,
};
use locality_notion::client::{HttpNotionApi, NotionApi};
use locality_notion::dto::{
    BlockDto, BlockListDto, DataSourceDto, DatabaseDto, NotionPageBundle, PageDto, PageListDto,
    PagePropertyDto, PaginatedListDto, ParentDto, RichTextBlockDto, RichTextDto, SelectOptionDto,
    SyncedBlockDto, SyncedFromDto, TextRichTextDto, TitleBlockDto,
};
use locality_notion::media::resolve_media_href_with_content_root;
use locality_notion::oauth::{
    NotionOAuthBrokerCodeExchange, NotionOAuthToken, StoredNotionCredential,
};
use locality_notion::{NotionConfig, NotionConnector};
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, AutoSaveState, ConnectionId,
    ConnectionRecord, ConnectionRepository, ConnectorProfileId, ConnectorProfileRecord,
    ConnectorProfileRepository, CredentialStore, EntityRecord, EntityRepository,
    FileCredentialStore, FreshnessStateRecord, FreshnessStateRepository, HydrationJobRepository,
    InMemoryStateStore, JournalRepository, MountConfig, MountRepository, ProjectionMode,
    RemoteObservationRecord, RemoteObservationRepository, ShadowRepository, SqliteStateStore,
    VirtualMutationRepository, open_credential_store,
};
use localityd::execution::PushJob;
use localityd::hydration::{
    HydratedEntity, HydrationExecutor, HydrationOutcome, HydrationQueue, HydrationSource,
};
use localityd::push::{PushJobAction, execute_auto_save_push_job_with_content_root};
use localityd::reconcile::{
    DefaultFetchScheduleStrategy, ScheduledPullSource, reconcile_scheduled_pull,
};
use localityd::runtime::{apply_remote_observation, workspace_virtual_freshness_jobs};
use localityd::scheduler::{PullScheduler, PullSchedulerTick};
use localityd::source::{SourceAdapter, SourcePushValidator};
use localityd::virtual_fs::{
    ROOT_CONTAINER_IDENTIFIER, commit_virtual_fs_write, create_virtual_fs_directory,
    create_virtual_fs_file, materialize_virtual_fs_item_with_content_root,
    mount_point_directory_name, mount_point_identifier, refresh_virtual_fs_children,
    rename_virtual_fs_item, trash_virtual_fs_item, virtual_fs_children_with_content_root,
    virtual_fs_content_root,
};
use localityd::virtual_projection::{unwrap_identifier, virtual_projection_root_children};
use serde_json::{Value, json};

const LIVE_PARENT_ENV: &str = "LOCALITY_NOTION_LIVE_PARENT_PAGE";
const LIVE_CONNECTION_ENV: &str = "LOCALITY_NOTION_LIVE_CONNECTION_ID";
const TOKEN_ENV: &str = "NOTION_TOKEN";

#[test]
fn mount_pull_mid_page_insert_push_and_status_clean() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::new());
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount");

    let pull = run_pull(&mut store, &connector, &fixture.root).expect("pull");
    assert!(pull.ok);
    assert_eq!(pull.via, "cli");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read pulled page");
    assert!(original.contains("First paragraph."));
    fs::write(
        &page_path,
        original.replace(
            "First paragraph.\n\n",
            "First paragraph.\n\nJust Testing 101\n\n",
        ),
    )
    .expect("write local edit");

    let diff = run_diff(&store, &page_path).expect("diff");
    let plan = diff.plan.as_ref().expect("plan");
    assert_eq!(diff.action, "confirm_plan");
    assert_eq!(plan.summary.blocks_created, 1);
    assert_eq!(plan.summary.blocks_moved, 0);

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push");
    assert!(push.ok);
    assert_eq!(push.action, "reconciled");

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status");
    assert!(status.clean, "{status:#?}");
    assert_eq!(status.summary.dirty, 0);

    let calls = api.calls.lock().expect("calls");
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, WriteCall::Append { .. }))
    );
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, WriteCall::Move { .. }))
    );
}

#[test]
fn push_journals_link_consecutive_edits_for_same_page() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::new());
    let connector = NotionConnector::with_api(NotionConfig::default(), api);

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount");
    run_pull(&mut store, &connector, &fixture.root).expect("pull");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read pulled page");
    fs::write(
        &page_path,
        original.replace("First paragraph.", "First pushed paragraph."),
    )
    .expect("write first edit");
    let first_push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("first push");
    let first_push_id = PushId(first_push.push_id.expect("first push id"));

    let after_first = fs::read_to_string(&page_path).expect("read first reconcile");
    fs::write(
        &page_path,
        after_first.replace("First pushed paragraph.", "Second pushed paragraph."),
    )
    .expect("write second edit");
    let second_push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("second push");
    let second_push_id = PushId(second_push.push_id.expect("second push id"));

    let journals = store.list_journal().expect("journals");
    let second = journals
        .iter()
        .find(|entry| entry.push_id == second_push_id)
        .expect("second journal");
    assert_eq!(second.metadata.previous_push_id, Some(first_push_id));
    assert_eq!(second.metadata.author.display_name, "anonymous");
    let readable = second
        .readable_diff
        .as_ref()
        .expect("journal readable diff");
    assert!(
        readable.text.contains("diff --locality"),
        "{}",
        readable.text
    );
    assert!(
        readable.text.contains("-First pushed paragraph."),
        "{}",
        readable.text
    );
    assert!(
        readable.text.contains("+Second pushed paragraph."),
        "{}",
        readable.text
    );
}

#[test]
fn mount_agent_guidance_matches_filesystem_workflow_and_does_not_dirty_status() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let connector =
        NotionConnector::with_api(NotionConfig::default(), Arc::new(MutableNotionApi::new()));

    let report = run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount");

    assert_eq!(
        report.guidance.agents_md.action,
        GuidanceFileAction::Created
    );
    assert!(matches!(
        report.guidance.claude_md.action,
        GuidanceFileAction::Symlinked | GuidanceFileAction::Copied
    ));

    let agents_path = fixture.root.join("AGENTS.md");
    let claude_path = fixture.root.join("CLAUDE.md");
    let agents = fs::read_to_string(&agents_path).expect("read mounted AGENTS.md");
    let claude = fs::read_to_string(&claude_path).expect("read mounted CLAUDE.md");
    assert_eq!(claude, agents);
    for expected in [
        "Browse directories normally",
        "Edit `page.md` for the page body",
        "loc create page --title",
        "parent-page/new-page/page.md",
        "database/new-row.md",
        "`_schema.yaml` files are read-only references",
        "loc status <path>",
        "loc diff <path>",
        "Use `loc push <path>` to make Notion match local edits",
        "If desktop Live Mode is on",
        "Do not run routine `loc pull` or `loc push` after every edit",
        "untrusted remote data",
    ] {
        assert!(
            agents.contains(expected),
            "mounted AGENTS.md should include `{expected}`:\n{agents}"
        );
    }
    assert!(
        agents.lines().count() <= 40,
        "agent guidance must stay short enough for agents to read:\n{agents}"
    );
    assert!(
        agents.split_whitespace().count() <= 450,
        "agent guidance must stay concise:\n{agents}"
    );

    let pull = run_pull(&mut store, &connector, &fixture.root).expect("pull mounted page");
    assert!(pull.ok);
    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status after guidance and pull");
    assert!(status.clean, "{status:#?}");
    assert_eq!(status.summary.dirty, 0, "{status:#?}");
    assert!(
        status
            .mounts
            .iter()
            .flat_map(|mount| mount.entries.iter())
            .all(|entry| {
                !entry.path.ends_with("AGENTS.md") && !entry.path.ends_with("CLAUDE.md")
            }),
        "agent guidance files are local instructions, not synced entities: {status:#?}"
    );

    let custom = E2eFixture::new();
    let mut custom_store = InMemoryStateStore::new();
    fs::create_dir_all(&custom.root).expect("create custom guidance mount root");
    fs::write(
        custom.root.join("AGENTS.md"),
        "# Custom\n\nProject-specific rules.\n",
    )
    .expect("write custom AGENTS.md");

    let custom_report = run_mount(
        &mut custom_store,
        MountOptions {
            mount_id: custom.mount_id.clone(),
            connector: "notion".to_string(),
            root: custom.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount with custom guidance");

    assert_eq!(
        custom_report.guidance.agents_md.action,
        GuidanceFileAction::Preserved
    );
    assert_eq!(
        fs::read_to_string(custom.root.join("AGENTS.md")).expect("read custom AGENTS.md"),
        "# Custom\n\nProject-specific rules.\n"
    );
    assert_eq!(
        fs::read_to_string(custom.root.join("CLAUDE.md")).expect("read custom CLAUDE.md"),
        "# Custom\n\nProject-specific rules.\n"
    );
}

#[test]
fn remote_delete_observation_e2e_removes_unopened_online_only_page() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount");

    let remote_id = RemoteId::new("page-1");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                remote_id.clone(),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Stub)
            .with_remote_edited_at("remote-v1"),
        )
        .expect("save entity");
    store
        .save_freshness_state(FreshnessStateRecord::new(
            fixture.mount_id.clone(),
            remote_id.clone(),
            FreshnessTier::Cold,
        ))
        .expect("save freshness");

    apply_remote_observation(
        &mut store,
        observe_job(&fixture.mount_id, &remote_id),
        remote_observation(&fixture.mount_id, &remote_id, true, "remote-v1"),
    )
    .expect("apply deleted observation");

    assert!(
        store
            .get_entity(&fixture.mount_id, &remote_id)
            .expect("get entity")
            .is_none(),
        "unopened online-only deleted pages should disappear without review"
    );
    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status");
    assert!(status.clean, "{status:#?}");
    assert_eq!(status.summary.total, 0, "{status:#?}");
}

#[test]
fn remote_delete_observation_e2e_removes_clean_hydrated_page() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::new());
    let connector = NotionConnector::with_api(NotionConfig::default(), api);

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount");
    run_pull(&mut store, &connector, &fixture.root).expect("initial pull");

    let page_path = fixture.page_file();
    let remote_id = RemoteId::new("page-1");
    let synced_version = store
        .get_entity(&fixture.mount_id, &remote_id)
        .expect("get entity")
        .expect("entity")
        .remote_edited_at
        .expect("synced remote version");

    apply_remote_observation(
        &mut store,
        observe_job(&fixture.mount_id, &remote_id),
        remote_observation(&fixture.mount_id, &remote_id, true, &synced_version),
    )
    .expect("apply deleted observation");

    assert!(
        !page_path.exists(),
        "clean materialized remote-deleted page should be removed"
    );
    assert!(
        store
            .get_entity(&fixture.mount_id, &remote_id)
            .expect("get entity")
            .is_none()
    );
    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status");
    assert!(status.clean, "{status:#?}");
    assert_eq!(status.summary.total, 0, "{status:#?}");
}

#[test]
fn remote_delete_observation_e2e_review_then_restored_check_clears_remote_deleted_issue() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount");

    let remote_id = RemoteId::new("page-1");
    let synced_version = "remote-v1";
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                remote_id.clone(),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Stub)
            .with_remote_edited_at(synced_version),
        )
        .expect("save entity");
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                fixture.mount_id.clone(),
                remote_id.clone(),
                FreshnessTier::Cold,
            )
            .opened_at("now"),
        )
        .expect("save freshness");

    apply_remote_observation(
        &mut store,
        observe_job(&fixture.mount_id, &remote_id),
        remote_observation(&fixture.mount_id, &remote_id, true, synced_version),
    )
    .expect("apply deleted observation");

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("deleted status");
    let entry = status.mounts[0].entries.first().expect("status entry");
    assert_eq!(entry.sync_state, StatusSyncState::ReviewNeeded);
    assert!(
        entry
            .issues
            .iter()
            .any(|issue| issue.code == "remote_deleted"),
        "{entry:#?}"
    );

    apply_remote_observation(
        &mut store,
        observe_job(&fixture.mount_id, &remote_id),
        remote_observation(&fixture.mount_id, &remote_id, false, synced_version),
    )
    .expect("apply restored observation");

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("restored status");
    assert!(
        status
            .mounts
            .iter()
            .flat_map(|mount| mount.entries.iter())
            .flat_map(|entry| entry.issues.iter())
            .all(|issue| issue.code != "remote_deleted"),
        "{status:#?}"
    );
}

#[test]
fn pull_materializes_and_repairs_downloaded_media_cache() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let image_bytes = b"locality-e2e-image-bytes".to_vec();
    let media_server = LocalMediaServer::new(image_bytes.clone(), 2);
    let api = Arc::new(MutableNotionApi::with_blocks(vec![
        paragraph_block("block-1", "Media cache page."),
        media_block(
            "image-block",
            "image",
            media_server.url(),
            "Local test image",
        ),
    ]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount media cache fixture");

    let pull = run_pull(&mut store, &connector, &fixture.root).expect("pull media cache page");
    assert!(pull.ok, "{pull:#?}");
    assert_eq!(pull.hydrated, 1, "{pull:#?}");

    let page_path = fixture.page_file();
    let markdown = fs::read_to_string(&page_path).expect("read media cache page");
    assert_local_image_markdown(&markdown, "Local test image");
    let local_image = local_image_path(&fixture.root, &page_path, &markdown, "Local test image");
    assert_eq!(
        fs::read(&local_image).expect("read materialized image"),
        image_bytes
    );
    let manifest = fs::read_to_string(fixture.root.join(".loc/media/manifest.json"))
        .expect("read media manifest");
    assert!(manifest.contains("image-block"), "{manifest}");
    assert!(manifest.contains(media_server.url()), "{manifest}");

    fs::remove_file(&local_image).expect("remove materialized image");
    let repair = run_pull(&mut store, &connector, &fixture.root).expect("repair media cache page");
    assert!(repair.ok, "{repair:#?}");
    assert_eq!(repair.hydrated, 1, "{repair:#?}");
    let repaired_markdown = fs::read_to_string(&page_path).expect("read repaired media cache page");
    let repaired_image = local_image_path(
        &fixture.root,
        &page_path,
        &repaired_markdown,
        "Local test image",
    );
    assert_eq!(
        fs::read(&repaired_image).expect("read repaired image"),
        image_bytes
    );

    {
        let mut blocks = api.blocks.lock().expect("media cache blocks");
        *blocks = vec![paragraph_block(
            "block-1",
            "Media cache page without image.",
        )];
    }
    let prune = run_pull(&mut store, &connector, &fixture.root).expect("prune media cache page");
    assert!(prune.ok, "{prune:#?}");
    let pruned_markdown = fs::read_to_string(&page_path).expect("read pruned media cache page");
    assert!(
        !pruned_markdown.contains("Local test image"),
        "{pruned_markdown}"
    );
    assert!(
        !local_image.exists(),
        "stale media file should be removed after clean pull no longer references it: {}",
        local_image.display()
    );
    assert!(
        !repaired_image.exists(),
        "repaired media file should be removed after clean pull no longer references it: {}",
        repaired_image.display()
    );
    let pruned_manifest = fs::read_to_string(fixture.root.join(".loc/media/manifest.json"))
        .expect("read pruned media manifest");
    assert!(
        !pruned_manifest.contains("image-block"),
        "{pruned_manifest}"
    );
    assert!(
        !pruned_manifest.contains(&media_server.url()),
        "{pruned_manifest}"
    );
    media_server.assert_served();
}

#[test]
fn oversized_local_media_append_fails_without_connector_apply_and_records_failed_journal() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
        "block-1",
        "Media upload guardrail base.",
    )]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount oversized media upload guardrail fixture");
    run_pull(&mut store, &connector, &fixture.root).expect("pull oversized media upload page");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read oversized media page");
    let media_dir = fixture.root.join(".loc/media/oversized");
    fs::create_dir_all(&media_dir).expect("create oversized media dir");
    let large_pdf = media_dir.join("large.pdf");
    let file = fs::File::create(&large_pdf).expect("create oversized pdf");
    file.set_len(20 * 1024 * 1024 + 1)
        .expect("size oversized pdf");
    drop(file);

    fs::write(
        &page_path,
        format!("{original}\n[Oversized PDF]({})\n", large_pdf.display()),
    )
    .expect("append oversized media link");

    let diff = run_diff(&store, &page_path).expect("diff oversized media append");
    let plan = diff.plan.as_ref().expect("oversized media append plan");
    assert!(diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    assert_eq!(plan.summary.blocks_created, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push oversized media append");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "apply_failed", "{push:#?}");
    assert_eq!(push.journal_status.as_deref(), Some("failed"), "{push:#?}");
    assert!(push.push_id.is_some(), "{push:#?}");
    assert!(
        push.message
            .as_deref()
            .unwrap_or_default()
            .contains("local media uploads larger than 20MB"),
        "{push:#?}"
    );
    let journals = store.list_journal().expect("journal");
    assert_eq!(journals.len(), 1, "{journals:#?}");
    assert!(
        matches!(
            &journals[0].status,
            JournalStatus::Failed(message)
                if message.contains("local media uploads larger than 20MB")
        ),
        "{journals:#?}"
    );
    let calls = api.calls.lock().expect("calls");
    assert!(
        calls.is_empty(),
        "oversized media upload must fail before upload or append: {calls:#?}"
    );
}

#[test]
fn multi_data_source_database_row_create_blocks_before_journaled_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
        "block-1",
        "Database guardrail root.",
    )]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount multi-data-source row create guardrail fixture");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("database-1"),
            EntityKind::Database,
            "Tasks",
            "Tasks",
        ))
        .expect("save database entity");
    let database_dir = fixture.root.join("Tasks");
    fs::create_dir_all(&database_dir).expect("create database directory");
    fs::write(database_dir.join("_schema.yaml"), ambiguous_tasks_schema())
        .expect("write ambiguous database schema");
    let new_row_dir = database_dir.join("new-task");
    fs::create_dir_all(&new_row_dir).expect("create new row directory");
    let new_row_path = new_row_dir.join("page.md");
    fs::write(
        &new_row_path,
        "---\ntitle: New ambiguous task\nStatus: Todo\n---\n# Notes\n\nCreated under an ambiguous database schema.\n",
    )
    .expect("write ambiguous row create");

    let diff = run_diff(&store, &new_row_path).expect("diff ambiguous row create");
    assert!(!diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(
        diff.validation[0].code,
        "notion_schema_ambiguous_data_source"
    );
    assert_eq!(diff.validation[0].file, "Tasks/new-task/page.md");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &new_row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push ambiguous row create");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert!(push.plan.is_none(), "{push:#?}");
    assert_eq!(
        push.validation[0].code,
        "notion_schema_ambiguous_data_source"
    );
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(
        store.list_journal().expect("journal").is_empty(),
        "ambiguous row create must not create a journal entry"
    );
    let calls = api.calls.lock().expect("calls");
    assert!(
        calls.is_empty(),
        "ambiguous row create must block before connector apply: {calls:#?}"
    );
    assert!(
        new_row_path.exists(),
        "blocked row create should not reconcile or move the draft file"
    );
}

#[test]
fn database_row_create_missing_schema_push_repairs_before_plan() {
    for (name, relative_path) in [
        ("direct-file", "Tasks/new-task.md"),
        ("page-document", "Tasks/new-task/page.md"),
    ] {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        let database: DatabaseDto = serde_json::from_value(json!({
            "id": "database-1",
            "title": rich_text_json("Tasks"),
            "data_sources": [{ "id": "source-1", "name": "Tasks" }]
        }))
        .unwrap_or_else(|error| panic!("{name}: database fixture: {error}"));
        let data_source: DataSourceDto = serde_json::from_value(json!({
            "id": "source-1",
            "name": "Tasks",
            "properties": {
                "Name": {
                    "id": "name",
                    "type": "title"
                },
                "Status": {
                    "id": "status",
                    "type": "select",
                    "select": {
                        "options": [
                            { "id": "todo", "name": "Todo" }
                        ]
                    }
                }
            }
        }))
        .unwrap_or_else(|error| panic!("{name}: data source fixture: {error}"));
        let api = Arc::new(MutableNotionApi::with_page_blocks_and_database_schema(
            page("page-1", "Database row create missing-schema root"),
            vec![paragraph_block(
                "block-1",
                "Database row create missing-schema root.",
            )],
            database,
            data_source,
        ));
        let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

        run_mount(
            &mut store,
            MountOptions {
                mount_id: fixture.mount_id.clone(),
                connector: "notion".to_string(),
                root: fixture.root.clone(),
                remote_root_id: Some(RemoteId::new("page-1")),
                connection_id: Some(ConnectionId::new("work")),
                read_only: false,
                projection: ProjectionMode::PlainFiles,
                settings_json: "{}".to_string(),
            },
        )
        .unwrap_or_else(|error| {
            panic!("{name}: mount missing-schema row create fixture: {error:?}")
        });
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Tasks",
                "Tasks",
            ))
            .unwrap_or_else(|error| panic!("{name}: save database entity: {error:?}"));

        let database_dir = fixture.root.join("Tasks");
        fs::create_dir_all(&database_dir)
            .unwrap_or_else(|error| panic!("{name}: create database directory: {error:?}"));
        let schema_path = database_dir.join("_schema.yaml");
        assert!(
            !schema_path.exists(),
            "{name}: test fixture must start without a cached schema"
        );

        let new_row_path = fixture.root.join(relative_path);
        if let Some(parent) = new_row_path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("{name}: create row parent: {error:?}"));
        }
        fs::write(
            &new_row_path,
            render_canonical_markdown(&CanonicalDocument::new(
                "title: New task\nStatus: Todo\n",
                "New row body.",
            )),
        )
        .unwrap_or_else(|error| panic!("{name}: write missing-schema row create: {error:?}"));

        let diff = run_diff(&store, &new_row_path)
            .unwrap_or_else(|error| panic!("{name}: diff missing-schema row create: {error:?}"));
        assert!(!diff.ok, "{name}: {diff:#?}");
        assert_eq!(diff.action, "fix_validation", "{name}: {diff:#?}");
        assert!(diff.plan.is_none(), "{name}: {diff:#?}");
        assert_eq!(diff.validation[0].code, "notion_schema_missing");
        assert_eq!(diff.validation[0].file, relative_path);
        assert!(
            !schema_path.exists(),
            "{name}: diff must not repair schema without a connector"
        );

        let push = run_push_with_daemon(
            &mut store,
            &connector,
            &new_row_path,
            PushOptions {
                assume_yes: false,
                confirm_dangerous: false,
            },
        )
        .unwrap_or_else(|error| panic!("{name}: push missing-schema row create: {error:?}"));
        assert!(!push.ok, "{name}: {push:#?}");
        assert_eq!(push.action, "confirm_plan", "{name}: {push:#?}");
        let plan = push.plan.as_ref().expect("row create plan after repair");
        assert_eq!(plan.summary.entities_created, 1, "{name}: {plan:#?}");
        assert_eq!(push.journal_status, None, "{name}: {push:#?}");
        assert!(
            store.list_journal().expect("journal").is_empty(),
            "{name}: unapproved row create must not create a journal entry"
        );
        assert!(
            schema_path.exists(),
            "{name}: daemon-backed push should repair missing schema before planning"
        );
        let repaired_schema =
            fs::read_to_string(&schema_path).expect("read repaired row-create schema");
        for expected in [
            "database_id: \"database-1\"",
            "\"Status\":",
            "name: \"Todo\"",
        ] {
            assert!(
                repaired_schema.contains(expected),
                "{name}: missing {expected:?}\n{repaired_schema}"
            );
        }
        let calls = api.calls.lock().expect("calls");
        assert!(
            calls.is_empty(),
            "{name}: schema repair and unapproved plan must not call connector apply: {calls:#?}"
        );
        assert!(
            new_row_path.exists(),
            "{name}: unapproved row create should leave the draft file"
        );
    }
}

#[test]
fn database_row_create_unknown_property_blocks_before_journaled_apply() {
    for (name, relative_path) in [
        ("direct-file", "Tasks/new-task.md"),
        ("page-document", "Tasks/new-task/page.md"),
    ] {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
            "block-1",
            "Database row create unknown property root.",
        )]));
        let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

        run_mount(
            &mut store,
            MountOptions {
                mount_id: fixture.mount_id.clone(),
                connector: "notion".to_string(),
                root: fixture.root.clone(),
                remote_root_id: Some(RemoteId::new("page-1")),
                connection_id: Some(ConnectionId::new("work")),
                read_only: false,
                projection: ProjectionMode::PlainFiles,
                settings_json: "{}".to_string(),
            },
        )
        .unwrap_or_else(|error| panic!("{name}: mount unknown row create fixture: {error:?}"));
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Tasks",
                "Tasks",
            ))
            .unwrap_or_else(|error| panic!("{name}: save database entity: {error:?}"));

        let database_dir = fixture.root.join("Tasks");
        fs::create_dir_all(&database_dir)
            .unwrap_or_else(|error| panic!("{name}: create database directory: {error:?}"));
        fs::write(
            database_dir.join("_schema.yaml"),
            optional_property_tasks_schema(),
        )
        .unwrap_or_else(|error| panic!("{name}: write database schema: {error:?}"));

        let new_row_path = fixture.root.join(relative_path);
        if let Some(parent) = new_row_path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("{name}: create row parent: {error:?}"));
        }
        fs::write(
            &new_row_path,
            render_canonical_markdown(&CanonicalDocument::new(
                "title: New task\nStatus: Todo\nUnexpected: value\n",
                "New row body.",
            )),
        )
        .unwrap_or_else(|error| panic!("{name}: write unknown property row create: {error:?}"));

        let diff = run_diff(&store, &new_row_path)
            .unwrap_or_else(|error| panic!("{name}: diff unknown property row create: {error:?}"));
        assert!(!diff.ok, "{name}: {diff:#?}");
        assert_eq!(diff.action, "fix_validation", "{name}: {diff:#?}");
        assert!(diff.plan.is_none(), "{name}: {diff:#?}");
        assert_eq!(
            diff.validation[0].code, "notion_schema_property_unknown",
            "{name}: {diff:#?}"
        );
        assert_eq!(diff.validation[0].file, relative_path, "{name}: {diff:#?}");

        let push = run_push_with_daemon(
            &mut store,
            &connector,
            &new_row_path,
            PushOptions {
                assume_yes: true,
                confirm_dangerous: false,
            },
        )
        .unwrap_or_else(|error| panic!("{name}: push unknown property row create: {error:?}"));
        assert!(!push.ok, "{name}: {push:#?}");
        assert_eq!(push.action, "fix_validation", "{name}: {push:#?}");
        assert!(push.plan.is_none(), "{name}: {push:#?}");
        assert_eq!(
            push.validation[0].code, "notion_schema_property_unknown",
            "{name}: {push:#?}"
        );
        assert_eq!(push.push_id, None, "{name}: {push:#?}");
        assert_eq!(push.journal_status, None, "{name}: {push:#?}");
        assert!(
            store.list_journal().expect("journal").is_empty(),
            "{name}: unknown property row create must not create a journal entry"
        );
        let calls = api.calls.lock().expect("calls");
        assert!(
            calls.is_empty(),
            "{name}: unknown property row create must block before connector apply: {calls:#?}"
        );
        assert!(
            new_row_path.exists(),
            "{name}: blocked row create should leave the draft file for correction"
        );
    }
}

#[test]
fn database_row_create_invalid_property_values_block_before_journaled_apply() {
    for (name, relative_path) in [
        ("direct-file", "Tasks/new-task.md"),
        ("page-document", "Tasks/new-task/page.md"),
    ] {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
            "block-1",
            "Database row create invalid property root.",
        )]));
        let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

        run_mount(
            &mut store,
            MountOptions {
                mount_id: fixture.mount_id.clone(),
                connector: "notion".to_string(),
                root: fixture.root.clone(),
                remote_root_id: Some(RemoteId::new("page-1")),
                connection_id: Some(ConnectionId::new("work")),
                read_only: false,
                projection: ProjectionMode::PlainFiles,
                settings_json: "{}".to_string(),
            },
        )
        .unwrap_or_else(|error| panic!("{name}: mount invalid row create fixture: {error:?}"));
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Tasks",
                "Tasks",
            ))
            .unwrap_or_else(|error| panic!("{name}: save database entity: {error:?}"));

        let database_dir = fixture.root.join("Tasks");
        fs::create_dir_all(&database_dir)
            .unwrap_or_else(|error| panic!("{name}: create database directory: {error:?}"));
        fs::write(
            database_dir.join("_schema.yaml"),
            optional_property_tasks_schema(),
        )
        .unwrap_or_else(|error| panic!("{name}: write database schema: {error:?}"));

        let new_row_path = fixture.root.join(relative_path);
        if let Some(parent) = new_row_path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("{name}: create row parent: {error:?}"));
        }
        fs::write(
            &new_row_path,
            render_canonical_markdown(&CanonicalDocument::new(
                "title: New task\nStatus: Blocked\nPoints: definitely not a number\nDue:\n  start: 13\nURL: ftp://example.com/task\nFiles:\n  - not-a-url\nPeople:\n  - Ada Lovelace\nRelated:\n  - bad-id\n",
                "New row body.",
            )),
        )
        .unwrap_or_else(|error| panic!("{name}: write invalid property row create: {error:?}"));

        let diff = run_diff(&store, &new_row_path)
            .unwrap_or_else(|error| panic!("{name}: diff invalid property row create: {error:?}"));
        assert!(!diff.ok, "{name}: {diff:#?}");
        assert_eq!(diff.action, "fix_validation", "{name}: {diff:#?}");
        assert!(diff.plan.is_none(), "{name}: {diff:#?}");
        let mut codes: Vec<&str> = diff
            .validation
            .iter()
            .map(|issue| issue.code.as_str())
            .collect();
        codes.sort_unstable();
        assert_eq!(
            codes,
            vec![
                "notion_schema_option_unknown",
                "notion_schema_property_number_invalid",
                "notion_schema_property_shape_invalid",
                "notion_schema_property_shape_invalid",
                "notion_schema_property_shape_invalid",
                "notion_schema_property_shape_invalid",
                "notion_schema_property_type_mismatch",
            ],
            "{name}: {diff:#?}"
        );

        let push = run_push_with_daemon(
            &mut store,
            &connector,
            &new_row_path,
            PushOptions {
                assume_yes: true,
                confirm_dangerous: false,
            },
        )
        .unwrap_or_else(|error| panic!("{name}: push invalid property row create: {error:?}"));
        assert!(!push.ok, "{name}: {push:#?}");
        assert_eq!(push.action, "fix_validation", "{name}: {push:#?}");
        assert!(push.plan.is_none(), "{name}: {push:#?}");
        assert_eq!(push.validation.len(), 7, "{name}: {push:#?}");
        assert_eq!(push.push_id, None, "{name}: {push:#?}");
        assert_eq!(push.journal_status, None, "{name}: {push:#?}");
        assert!(
            store.list_journal().expect("journal").is_empty(),
            "{name}: invalid property row create must not create a journal entry"
        );
        let calls = api.calls.lock().expect("calls");
        assert!(
            calls.is_empty(),
            "{name}: invalid property row create must block before connector apply: {calls:#?}"
        );
        assert!(
            new_row_path.exists(),
            "{name}: blocked row create should leave the draft file for correction"
        );
    }
}

#[test]
fn database_row_create_read_only_property_blocks_before_journaled_apply() {
    for (name, relative_path) in [
        ("direct-file", "Tasks/new-task.md"),
        ("page-document", "Tasks/new-task/page.md"),
    ] {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
            "block-1",
            "Database row create read-only property root.",
        )]));
        let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

        run_mount(
            &mut store,
            MountOptions {
                mount_id: fixture.mount_id.clone(),
                connector: "notion".to_string(),
                root: fixture.root.clone(),
                remote_root_id: Some(RemoteId::new("page-1")),
                connection_id: Some(ConnectionId::new("work")),
                read_only: false,
                projection: ProjectionMode::PlainFiles,
                settings_json: "{}".to_string(),
            },
        )
        .unwrap_or_else(|error| panic!("{name}: mount read-only row create fixture: {error:?}"));
        store
            .save_entity(EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("database-1"),
                EntityKind::Database,
                "Tasks",
                "Tasks",
            ))
            .unwrap_or_else(|error| panic!("{name}: save database entity: {error:?}"));

        let database_dir = fixture.root.join("Tasks");
        fs::create_dir_all(&database_dir)
            .unwrap_or_else(|error| panic!("{name}: create database directory: {error:?}"));
        fs::write(
            database_dir.join("_schema.yaml"),
            read_only_property_tasks_schema(),
        )
        .unwrap_or_else(|error| panic!("{name}: write database schema: {error:?}"));

        let new_row_path = fixture.root.join(relative_path);
        if let Some(parent) = new_row_path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("{name}: create row parent: {error:?}"));
        }
        fs::write(
            &new_row_path,
            render_canonical_markdown(&CanonicalDocument::new(
                "title: New task\nStatus: Todo\nFormula: edited locally\n",
                "New row body.",
            )),
        )
        .unwrap_or_else(|error| panic!("{name}: write read-only property row create: {error:?}"));

        let diff = run_diff(&store, &new_row_path).unwrap_or_else(|error| {
            panic!("{name}: diff read-only property row create: {error:?}")
        });
        assert!(!diff.ok, "{name}: {diff:#?}");
        assert_eq!(diff.action, "fix_validation", "{name}: {diff:#?}");
        assert!(diff.plan.is_none(), "{name}: {diff:#?}");
        assert_eq!(
            diff.validation[0].code, "notion_schema_property_read_only",
            "{name}: {diff:#?}"
        );
        assert_eq!(diff.validation[0].file, relative_path, "{name}: {diff:#?}");

        let push = run_push_with_daemon(
            &mut store,
            &connector,
            &new_row_path,
            PushOptions {
                assume_yes: true,
                confirm_dangerous: false,
            },
        )
        .unwrap_or_else(|error| panic!("{name}: push read-only property row create: {error:?}"));
        assert!(!push.ok, "{name}: {push:#?}");
        assert_eq!(push.action, "fix_validation", "{name}: {push:#?}");
        assert!(push.plan.is_none(), "{name}: {push:#?}");
        assert_eq!(
            push.validation[0].code, "notion_schema_property_read_only",
            "{name}: {push:#?}"
        );
        assert_eq!(push.push_id, None, "{name}: {push:#?}");
        assert_eq!(push.journal_status, None, "{name}: {push:#?}");
        assert!(
            store.list_journal().expect("journal").is_empty(),
            "{name}: read-only property row create must not create a journal entry"
        );
        let calls = api.calls.lock().expect("calls");
        assert!(
            calls.is_empty(),
            "{name}: read-only property row create must block before connector apply: {calls:#?}"
        );
        assert!(
            new_row_path.exists(),
            "{name}: blocked row create should leave the draft file for correction"
        );
    }
}

#[test]
fn database_row_create_locality_metadata_blocks_before_journaled_apply() {
    let cases = vec![
        (
            "has-remote-id",
            "loc:\n  id: existing-row\n  type: page\ntitle: Has remote id\nStatus: Todo\n",
            "New database rows cannot claim an existing remote page.".to_string(),
            "create_entity_has_remote_id",
        ),
        (
            "type-not-page",
            "loc:\n  type: database\ntitle: Type not page\nStatus: Todo\n",
            "New database rows with Locality metadata must still be pages.".to_string(),
            "create_entity_type_not_page",
        ),
        (
            "missing-title",
            "Status: Todo\n",
            "New database rows need a title.".to_string(),
            "create_entity_missing_title",
        ),
        (
            "stub-body",
            "title: Stub body\nStatus: Todo\n",
            format!("{}\n", CanonicalDocument::STUB_MARKER),
            "create_entity_stub_body",
        ),
        (
            "directive",
            "title: Directive\nStatus: Todo\n",
            "::loc{id=seeded-block type=unsupported kind=\"unsupported\"}\n".to_string(),
            "create_entity_directive_unsupported",
        ),
    ];

    for (shape, relative_path) in [
        ("direct-file", "Tasks/new-task.md"),
        ("page-document", "Tasks/new-task/page.md"),
    ] {
        for (name, frontmatter, body, expected_code) in &cases {
            let fixture = E2eFixture::new();
            let mut store = InMemoryStateStore::new();
            let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
                "block-1",
                "Database row create Locality metadata root.",
            )]));
            let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

            run_mount(
                &mut store,
                MountOptions {
                    mount_id: fixture.mount_id.clone(),
                    connector: "notion".to_string(),
                    root: fixture.root.clone(),
                    remote_root_id: Some(RemoteId::new("page-1")),
                    connection_id: Some(ConnectionId::new("work")),
                    read_only: false,
                    projection: ProjectionMode::PlainFiles,
                    settings_json: "{}".to_string(),
                },
            )
            .unwrap_or_else(|error| {
                panic!("{shape}/{name}: mount Locality metadata row create fixture: {error:?}")
            });
            store
                .save_entity(EntityRecord::new(
                    fixture.mount_id.clone(),
                    RemoteId::new("database-1"),
                    EntityKind::Database,
                    "Tasks",
                    "Tasks",
                ))
                .unwrap_or_else(|error| panic!("{shape}/{name}: save database entity: {error:?}"));

            let database_dir = fixture.root.join("Tasks");
            fs::create_dir_all(&database_dir).unwrap_or_else(|error| {
                panic!("{shape}/{name}: create database directory: {error:?}")
            });
            fs::write(
                database_dir.join("_schema.yaml"),
                optional_property_tasks_schema(),
            )
            .unwrap_or_else(|error| panic!("{shape}/{name}: write database schema: {error:?}"));

            let new_row_path = fixture.root.join(relative_path);
            if let Some(parent) = new_row_path.parent() {
                fs::create_dir_all(parent)
                    .unwrap_or_else(|error| panic!("{shape}/{name}: create row parent: {error:?}"));
            }
            fs::write(
                &new_row_path,
                render_canonical_markdown(&CanonicalDocument::new(*frontmatter, body.as_str())),
            )
            .unwrap_or_else(|error| {
                panic!("{shape}/{name}: write Locality metadata row create: {error:?}")
            });

            let diff = run_diff(&store, &new_row_path).unwrap_or_else(|error| {
                panic!("{shape}/{name}: diff Locality metadata row create: {error:?}")
            });
            assert!(!diff.ok, "{shape}/{name}: {diff:#?}");
            assert_eq!(diff.action, "fix_validation", "{shape}/{name}: {diff:#?}");
            assert!(diff.plan.is_none(), "{shape}/{name}: {diff:#?}");
            assert_eq!(
                diff.validation[0].code, *expected_code,
                "{shape}/{name}: {diff:#?}"
            );
            assert_eq!(
                diff.validation[0].file, relative_path,
                "{shape}/{name}: {diff:#?}"
            );

            let push = run_push_with_daemon(
                &mut store,
                &connector,
                &new_row_path,
                PushOptions {
                    assume_yes: true,
                    confirm_dangerous: false,
                },
            )
            .unwrap_or_else(|error| {
                panic!("{shape}/{name}: push Locality metadata row create: {error:?}")
            });
            assert!(!push.ok, "{shape}/{name}: {push:#?}");
            assert_eq!(push.action, "fix_validation", "{shape}/{name}: {push:#?}");
            assert!(push.plan.is_none(), "{shape}/{name}: {push:#?}");
            assert_eq!(
                push.validation[0].code, *expected_code,
                "{shape}/{name}: {push:#?}"
            );
            assert_eq!(push.push_id, None, "{shape}/{name}: {push:#?}");
            assert_eq!(push.journal_status, None, "{shape}/{name}: {push:#?}");
            assert!(
                store.list_journal().expect("journal").is_empty(),
                "{shape}/{name}: Locality metadata row create must not create a journal entry"
            );
            let calls = api.calls.lock().expect("calls");
            assert!(
                calls.is_empty(),
                "{shape}/{name}: Locality metadata row create must block before connector apply: {calls:#?}"
            );
            assert!(
                new_row_path.exists(),
                "{shape}/{name}: blocked row create should leave the draft file for correction"
            );
        }
    }
}

#[test]
fn database_row_read_only_property_blocks_before_journaled_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
        "block-1",
        "Database read-only property root.",
    )]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount read-only property guardrail fixture");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("database-1"),
            EntityKind::Database,
            "Tasks",
            "Tasks",
        ))
        .expect("save database entity");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("row-1"),
                EntityKind::Page,
                "Existing task",
                "Tasks/existing-task/page.md",
            )
            .with_hydration(HydrationState::Hydrated)
            .with_remote_edited_at("2026-06-10T00:00:00.000Z"),
        )
        .expect("save database row entity");

    let database_dir = fixture.root.join("Tasks");
    let row_dir = database_dir.join("existing-task");
    fs::create_dir_all(&row_dir).expect("create database row directory");
    fs::write(
        database_dir.join("_schema.yaml"),
        read_only_property_tasks_schema(),
    )
    .expect("write read-only property database schema");

    let synced_frontmatter = "loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Existing task\nStatus: Todo\n";
    let edited_frontmatter = "loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Existing task\nStatus: Todo\nFormula: edited locally\n";
    let body = "Existing database row body.";
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("row-1"),
                body,
                10,
                [RemoteId::new("block-1")],
            )
            .expect("database row shadow")
            .with_frontmatter(synced_frontmatter),
        )
        .expect("save database row shadow");

    let row_path = row_dir.join("page.md");
    fs::write(
        &row_path,
        render_canonical_markdown(&CanonicalDocument::new(edited_frontmatter, body)),
    )
    .expect("write read-only property edit");

    let diff = run_diff(&store, &row_path).expect("diff read-only property edit");
    assert!(!diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(diff.validation[0].code, "notion_schema_property_read_only");
    assert_eq!(diff.validation[0].file, "Tasks/existing-task/page.md");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push read-only property edit");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert!(push.plan.is_none(), "{push:#?}");
    assert_eq!(push.validation[0].code, "notion_schema_property_read_only");
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(
        store.list_journal().expect("journal").is_empty(),
        "read-only property edit must not create a journal entry"
    );
    let calls = api.calls.lock().expect("calls");
    assert!(
        calls.is_empty(),
        "read-only property edit must block before connector apply: {calls:#?}"
    );
    assert!(
        fs::read_to_string(&row_path)
            .expect("read blocked row")
            .contains("Formula: edited locally"),
        "blocked row should remain for user correction"
    );
}

#[test]
fn database_row_unknown_property_blocks_before_journaled_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
        "block-1",
        "Database unknown property root.",
    )]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount unknown property guardrail fixture");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("database-1"),
            EntityKind::Database,
            "Tasks",
            "Tasks",
        ))
        .expect("save database entity");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("row-1"),
                EntityKind::Page,
                "Existing task",
                "Tasks/existing-task/page.md",
            )
            .with_hydration(HydrationState::Hydrated)
            .with_remote_edited_at("2026-06-10T00:00:00.000Z"),
        )
        .expect("save database row entity");

    let database_dir = fixture.root.join("Tasks");
    let row_dir = database_dir.join("existing-task");
    fs::create_dir_all(&row_dir).expect("create unknown property row directory");
    fs::write(
        database_dir.join("_schema.yaml"),
        optional_property_tasks_schema(),
    )
    .expect("write unknown property database schema");

    let synced_frontmatter = "loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Existing task\nStatus: Todo\n";
    let edited_frontmatter = "loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Existing task\nStatus: Todo\nUnexpected: edited locally\n";
    let body = "Existing database row body.";
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("row-1"),
                body,
                10,
                [RemoteId::new("block-1")],
            )
            .expect("unknown property row shadow")
            .with_frontmatter(synced_frontmatter),
        )
        .expect("save unknown property row shadow");

    let row_path = row_dir.join("page.md");
    fs::write(
        &row_path,
        render_canonical_markdown(&CanonicalDocument::new(edited_frontmatter, body)),
    )
    .expect("write unknown property edit");

    let diff = run_diff(&store, &row_path).expect("diff unknown property edit");
    assert!(!diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(diff.validation[0].code, "notion_schema_property_unknown");
    assert_eq!(diff.validation[0].file, "Tasks/existing-task/page.md");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push unknown property edit");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert!(push.plan.is_none(), "{push:#?}");
    assert_eq!(push.validation[0].code, "notion_schema_property_unknown");
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(
        store.list_journal().expect("journal").is_empty(),
        "unknown property edit must not create a journal entry"
    );
    let calls = api.calls.lock().expect("calls");
    assert!(
        calls.is_empty(),
        "unknown property edit must block before connector apply: {calls:#?}"
    );
    assert!(
        fs::read_to_string(&row_path)
            .expect("read blocked row")
            .contains("Unexpected: edited locally"),
        "blocked row should remain for user correction"
    );
}

#[test]
fn database_row_optional_property_removals_clear_remote_values() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let row_page: PageDto = serde_json::from_value(json!({
        "id": "row-1",
        "created_time": "2026-06-10T00:00:00.000Z",
        "last_edited_time": "2026-06-10T00:00:00.000Z",
        "properties": {
            "Name": {
                "type": "title",
                "title": rich_text_json("Existing task")
            },
            "Status": {
                "type": "select",
                "select": { "id": "todo", "name": "Todo" }
            },
            "Tags": {
                "type": "multi_select",
                "multi_select": [{ "id": "alpha", "name": "Alpha" }]
            },
            "Points": {
                "type": "number",
                "number": 5
            },
            "Due": {
                "type": "date",
                "date": { "start": "2026-06-13" }
            },
            "URL": {
                "type": "url",
                "url": "https://example.com/task"
            },
            "Files": {
                "type": "files",
                "files": [{
                    "name": "Spec",
                    "type": "external",
                    "external": { "url": "https://example.com/spec.pdf" }
                }]
            },
            "People": {
                "type": "people",
                "people": [{ "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "name": "Ada" }]
            },
            "Related": {
                "type": "relation",
                "relation": [{ "id": "11111111111111111111111111111111" }]
            }
        }
    }))
    .expect("optional property row page");
    let api = Arc::new(MutableNotionApi::with_page_and_blocks(
        row_page,
        vec![paragraph_block("block-1", "Existing database row body.")],
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount optional property clear fixture");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("database-1"),
            EntityKind::Database,
            "Tasks",
            "Tasks",
        ))
        .expect("save database entity");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("row-1"),
                EntityKind::Page,
                "Existing task",
                "Tasks/existing-task/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save database row entity");

    let database_dir = fixture.root.join("Tasks");
    let row_dir = database_dir.join("existing-task");
    fs::create_dir_all(&row_dir).expect("create optional property row directory");
    fs::write(
        database_dir.join("_schema.yaml"),
        optional_property_tasks_schema(),
    )
    .expect("write optional property database schema");

    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new("row-1"),
        })
        .expect("fetch optional property row fixture");
    let synced_document = connector
        .render(&native)
        .expect("render optional property row fixture");
    let edited_frontmatter = "loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: \"2026-06-10T00:00:00.000Z\"\ntitle: Existing task\n";
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("row-1"),
                synced_document.body.clone(),
                synced_document.frontmatter.lines().count() + 3,
                [RemoteId::new("block-1")],
            )
            .expect("optional property row shadow")
            .with_frontmatter(synced_document.frontmatter),
        )
        .expect("save optional property row shadow");

    let row_path = row_dir.join("page.md");
    fs::write(
        &row_path,
        render_canonical_markdown(&CanonicalDocument::new(
            edited_frontmatter,
            synced_document.body,
        )),
    )
    .expect("write optional property removals");

    let diff = run_diff(&store, &row_path).expect("diff optional property removals");
    assert!(diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("optional property clear plan");
    assert_eq!(plan.summary.properties_updated, 8, "{plan:#?}");
    assert_eq!(plan.summary.blocks_updated, 0, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push optional property removals");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let calls = api.calls.lock().expect("calls");
    assert_eq!(
        calls.as_slice(),
        [WriteCall::UpdatePage {
            page_id: "row-1".to_string(),
            body: json!({
                "properties": {
                    "Status": { "select": Value::Null },
                    "Tags": { "multi_select": [] },
                    "Points": { "number": Value::Null },
                    "Due": { "date": Value::Null },
                    "URL": { "url": Value::Null },
                    "Files": { "files": [] },
                    "People": { "people": [] },
                    "Related": { "relation": [] },
                },
            }),
        }],
        "optional property removals must be sent as explicit clears"
    );

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(row_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status after optional property clear");
    assert!(clean_status.clean, "{clean_status:#?}");
    let reconciled = fs::read_to_string(&row_path).expect("read reconciled optional property row");
    for expected in [
        "\"Status\": null",
        "\"Tags\": []",
        "\"Points\": null",
        "\"Due\": null",
        "\"URL\": null",
        "\"Files\": []",
        "\"People\": []",
        "\"Related\": []",
    ] {
        assert!(
            reconciled.contains(expected),
            "missing {expected:?}\n{reconciled}"
        );
    }
}

#[test]
fn database_row_missing_schema_diff_blocks_and_push_repairs_before_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let row_page: PageDto = serde_json::from_value(json!({
        "id": "row-1",
        "created_time": "2026-06-10T00:00:00.000Z",
        "last_edited_time": "2026-06-10T00:00:00.000Z",
        "properties": {
            "Name": {
                "type": "title",
                "title": rich_text_json("Existing task")
            },
            "Status": {
                "type": "select",
                "select": { "id": "todo", "name": "Todo" }
            }
        }
    }))
    .expect("missing-schema row page");
    let database: DatabaseDto = serde_json::from_value(json!({
        "id": "database-1",
        "title": rich_text_json("Tasks"),
        "data_sources": [{ "id": "source-1", "name": "Tasks" }]
    }))
    .expect("missing-schema database");
    let data_source: DataSourceDto = serde_json::from_value(json!({
        "id": "source-1",
        "name": "Tasks",
        "properties": {
            "Name": {
                "id": "name",
                "type": "title"
            },
            "Status": {
                "id": "status",
                "type": "select",
                "select": {
                    "options": [
                        { "id": "todo", "name": "Todo" },
                        { "id": "done", "name": "Done" }
                    ]
                }
            }
        }
    }))
    .expect("missing-schema data source");
    let api = Arc::new(MutableNotionApi::with_page_blocks_and_database_schema(
        row_page,
        vec![paragraph_block("block-1", "Existing database row body.")],
        database,
        data_source,
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount missing-schema repair fixture");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("database-1"),
            EntityKind::Database,
            "Tasks",
            "Tasks",
        ))
        .expect("save database entity");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("row-1"),
                EntityKind::Page,
                "Existing task",
                "Tasks/existing-task/page.md",
            )
            .with_hydration(HydrationState::Hydrated)
            .with_remote_edited_at("2026-06-10T00:00:00.000Z"),
        )
        .expect("save database row entity");

    let database_dir = fixture.root.join("Tasks");
    let row_dir = database_dir.join("existing-task");
    fs::create_dir_all(&row_dir).expect("create missing-schema row directory");
    let schema_path = database_dir.join("_schema.yaml");
    assert!(
        !schema_path.exists(),
        "test fixture must start without a cached database schema"
    );

    let synced_document = connector
        .render(
            &connector
                .fetch(FetchRequest {
                    remote_id: RemoteId::new("row-1"),
                })
                .expect("fetch missing-schema row fixture"),
        )
        .expect("render missing-schema row fixture");
    let body = synced_document.body.clone();
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("row-1"),
                body.clone(),
                synced_document.frontmatter.lines().count() + 3,
                [RemoteId::new("block-1")],
            )
            .expect("missing-schema row shadow")
            .with_frontmatter(synced_document.frontmatter),
        )
        .expect("save missing-schema row shadow");

    let row_path = row_dir.join("page.md");
    fs::write(
        &row_path,
        render_canonical_markdown(&CanonicalDocument::new(
            "loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: \"2026-06-10T00:00:00.000Z\"\ntitle: Existing task\nStatus: Done\n",
            body,
        )),
    )
    .expect("write missing-schema row edit");

    let diff = run_diff(&store, &row_path).expect("diff missing-schema row edit");
    assert!(!diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(diff.validation[0].code, "notion_schema_missing");
    assert_eq!(diff.validation[0].file, "Tasks/existing-task/page.md");
    assert!(
        store.list_journal().expect("journal").is_empty(),
        "diff-only missing schema validation must not create a journal entry"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push missing-schema row edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(
        push.journal_status.as_deref(),
        Some("reconciled"),
        "{push:#?}"
    );
    assert!(
        schema_path.exists(),
        "daemon-backed push should repair the missing schema cache before validation"
    );
    let repaired_schema = fs::read_to_string(&schema_path).expect("read repaired database schema");
    for expected in [
        "database_id: \"database-1\"",
        "id: \"source-1\"",
        "\"Status\":",
        "name: \"Done\"",
    ] {
        assert!(
            repaired_schema.contains(expected),
            "missing {expected:?}\n{repaired_schema}"
        );
    }

    let calls = api.calls.lock().expect("calls");
    assert_eq!(
        calls.as_slice(),
        [WriteCall::UpdatePage {
            page_id: "row-1".to_string(),
            body: json!({
                "properties": {
                    "Status": { "select": { "name": "Done" } },
                },
            }),
        }],
        "schema repair must happen before exactly one connector property update"
    );

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(row_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status after missing-schema repair push");
    assert!(clean_status.clean, "{clean_status:#?}");
    let reconciled = fs::read_to_string(&row_path).expect("read reconciled missing-schema row");
    assert!(reconciled.contains("\"Status\": \"Done\""), "{reconciled}");
}

#[test]
fn database_row_people_relation_id_shapes_block_before_journaled_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
        "block-1",
        "Database ID-shaped property root.",
    )]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount ID-shaped property guardrail fixture");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("database-1"),
            EntityKind::Database,
            "Tasks",
            "Tasks",
        ))
        .expect("save database entity");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("row-1"),
                EntityKind::Page,
                "Existing task",
                "Tasks/existing-task/page.md",
            )
            .with_hydration(HydrationState::Hydrated)
            .with_remote_edited_at("2026-06-10T00:00:00.000Z"),
        )
        .expect("save database row entity");

    let database_dir = fixture.root.join("Tasks");
    let row_dir = database_dir.join("existing-task");
    fs::create_dir_all(&row_dir).expect("create ID-shaped property row directory");
    fs::write(
        database_dir.join("_schema.yaml"),
        optional_property_tasks_schema(),
    )
    .expect("write ID-shaped property database schema");

    let synced_frontmatter = "loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Existing task\nPeople:\n  - \"Ada <aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa>\"\nRelated:\n  - \"11111111111111111111111111111111\"\n";
    let edited_frontmatter = "loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Existing task\nPeople:\n  - Ada Lovelace\nRelated:\n  - Needs a page id\n";
    let body = "Existing database row body.";
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("row-1"),
                body,
                12,
                [RemoteId::new("block-1")],
            )
            .expect("ID-shaped property row shadow")
            .with_frontmatter(synced_frontmatter),
        )
        .expect("save ID-shaped property row shadow");

    let row_path = row_dir.join("page.md");
    fs::write(
        &row_path,
        render_canonical_markdown(&CanonicalDocument::new(edited_frontmatter, body)),
    )
    .expect("write invalid ID-shaped properties");

    let diff = run_diff(&store, &row_path).expect("diff invalid ID-shaped properties");
    assert!(!diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(diff.validation.len(), 2, "{diff:#?}");
    assert!(
        diff.validation
            .iter()
            .all(|issue| issue.code == "notion_schema_property_shape_invalid"),
        "{diff:#?}"
    );
    assert!(
        diff.validation
            .iter()
            .any(|issue| issue.message.contains("People")
                && issue.message.contains("valid Notion user IDs")),
        "{diff:#?}"
    );
    assert!(
        diff.validation
            .iter()
            .any(|issue| issue.message.contains("Related")
                && issue.message.contains("valid Notion page IDs")),
        "{diff:#?}"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push invalid ID-shaped properties");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert!(push.plan.is_none(), "{push:#?}");
    assert_eq!(push.validation.len(), 2, "{push:#?}");
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(
        store.list_journal().expect("journal").is_empty(),
        "invalid ID-shaped properties must not create a journal entry"
    );
    let calls = api.calls.lock().expect("calls");
    assert!(
        calls.is_empty(),
        "invalid ID-shaped properties must block before connector apply: {calls:#?}"
    );
    let blocked = fs::read_to_string(&row_path).expect("read invalid ID-shaped row");
    assert!(blocked.contains("Ada Lovelace"), "{blocked}");
    assert!(blocked.contains("Needs a page id"), "{blocked}");
}

#[test]
fn database_row_scalar_property_shapes_block_before_journaled_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
        "block-1",
        "Database scalar property root.",
    )]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount scalar property guardrail fixture");
    store
        .save_entity(EntityRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("database-1"),
            EntityKind::Database,
            "Tasks",
            "Tasks",
        ))
        .expect("save database entity");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("row-1"),
                EntityKind::Page,
                "Existing task",
                "Tasks/existing-task/page.md",
            )
            .with_hydration(HydrationState::Hydrated)
            .with_remote_edited_at("2026-06-10T00:00:00.000Z"),
        )
        .expect("save database row entity");

    let database_dir = fixture.root.join("Tasks");
    let row_dir = database_dir.join("existing-task");
    fs::create_dir_all(&row_dir).expect("create scalar property row directory");
    fs::write(
        database_dir.join("_schema.yaml"),
        optional_property_tasks_schema(),
    )
    .expect("write scalar property database schema");

    let synced_frontmatter = "loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Existing task\nStatus: Todo\nPoints: 5\nDue: \"2026-06-13\"\nURL: https://example.com/task\nFiles:\n  - Spec <https://example.com/spec.pdf>\n";
    let edited_frontmatter = "loc:\n  id: row-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Existing task\nStatus: Todo\nPoints: definitely not a number\nDue:\n  start: 13\nURL: ftp://example.com/task\nFiles:\n  - not-a-url\n";
    let body = "Existing database row body.";
    store
        .save_shadow(
            &fixture.mount_id,
            ShadowDocument::from_synced_body(
                RemoteId::new("row-1"),
                body,
                15,
                [RemoteId::new("block-1")],
            )
            .expect("scalar property row shadow")
            .with_frontmatter(synced_frontmatter),
        )
        .expect("save scalar property row shadow");

    let row_path = row_dir.join("page.md");
    fs::write(
        &row_path,
        render_canonical_markdown(&CanonicalDocument::new(edited_frontmatter, body)),
    )
    .expect("write invalid scalar properties");

    let diff = run_diff(&store, &row_path).expect("diff invalid scalar properties");
    assert!(!diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    let mut codes: Vec<&str> = diff
        .validation
        .iter()
        .map(|issue| issue.code.as_str())
        .collect();
    codes.sort_unstable();
    assert_eq!(
        codes,
        vec![
            "notion_schema_property_number_invalid",
            "notion_schema_property_shape_invalid",
            "notion_schema_property_shape_invalid",
            "notion_schema_property_type_mismatch",
        ],
        "{diff:#?}"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push invalid scalar properties");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert!(push.plan.is_none(), "{push:#?}");
    assert_eq!(push.validation.len(), 4, "{push:#?}");
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(
        store.list_journal().expect("journal").is_empty(),
        "invalid scalar properties must not create a journal entry"
    );
    let calls = api.calls.lock().expect("calls");
    assert!(
        calls.is_empty(),
        "invalid scalar properties must block before connector apply: {calls:#?}"
    );
    let blocked = fs::read_to_string(&row_path).expect("read invalid scalar row");
    assert!(blocked.contains("definitely not a number"), "{blocked}");
    assert!(blocked.contains("ftp://example.com/task"), "{blocked}");
    assert!(blocked.contains("not-a-url"), "{blocked}");
}

#[test]
fn property_only_push_journals_and_undo_reports_blocked_property_preimage() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let original_frontmatter = "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\nStatus: Todo\n";
    let edited_frontmatter = "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\nStatus: Done\n";
    let body = "Property-only body stays unchanged.";
    let connector = PropertyOnlyConnector::new(original_frontmatter, edited_frontmatter, body);

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "property-test".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount property-only undo fixture");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save property-only entity");
    let shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        body,
        10,
        [RemoteId::new("block-1")],
    )
    .expect("property-only shadow")
    .with_frontmatter(original_frontmatter);
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save property-only shadow");

    let page_path = fixture.root.join("Roadmap/page.md");
    fs::create_dir_all(page_path.parent().expect("page parent")).expect("create page parent");
    fs::write(
        &page_path,
        render_canonical_markdown(&CanonicalDocument::new(edited_frontmatter, body)),
    )
    .expect("write property-only edit");

    let diff = run_diff(&store, &page_path).expect("diff property-only edit");
    assert!(diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("property-only plan");
    assert_eq!(plan.summary.properties_updated, 1, "{plan:#?}");
    assert_eq!(plan.summary.blocks_updated, 0, "{plan:#?}");
    assert_eq!(plan.summary.blocks_created, 0, "{plan:#?}");
    assert_eq!(plan.summary.blocks_archived, 0, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push property-only edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(
        push.journal_status.as_deref(),
        Some("reconciled"),
        "{push:#?}"
    );
    assert_eq!(connector.apply_count(), 1);
    let push_id = push.push_id.clone().expect("property-only push id");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status after property-only push");
    assert!(clean_status.clean, "{clean_status:#?}");

    let undo = run_undo(&mut store, &push_id).expect("undo property-only push");
    assert!(!undo.ok, "{undo:#?}");
    assert_eq!(undo.action, "undo_plan_blocked", "{undo:#?}");
    assert_eq!(undo.status, "reconciled", "{undo:#?}");
    let undo_plan = undo.undo_plan.as_ref().expect("property-only undo plan");
    assert_eq!(undo_plan.status, "blocked", "{undo:#?}");
    assert!(undo_plan.operations.is_empty(), "{undo:#?}");
    assert_eq!(
        undo_plan.unsupported[0].code,
        "update_properties_missing_property_preimage"
    );
    assert_eq!(
        store
            .get_journal(&PushId(push_id))
            .expect("load property-only journal")
            .expect("property-only journal")
            .status,
        JournalStatus::Reconciled
    );
}

#[test]
fn new_page_create_validation_blocks_before_journaled_apply() {
    let cases = vec![
        (
            "has-remote-id",
            "Has Remote Id.md",
            render_canonical_markdown(&CanonicalDocument::new(
                "loc:\n  id: existing-page\n  type: page\ntitle: Has remote id\n",
                "New files cannot claim an existing remote page.\n",
            )),
            "create_entity_has_remote_id",
        ),
        (
            "type-not-page",
            "Type Not Page.md",
            render_canonical_markdown(&CanonicalDocument::new(
                "loc:\n  type: database\ntitle: Type not page\n",
                "New files with Locality metadata must be pages.\n",
            )),
            "create_entity_type_not_page",
        ),
        (
            "missing-title",
            "Missing Title.md",
            render_canonical_markdown(&CanonicalDocument::new(
                "loc:\n  type: page\n",
                "New files need a title.\n",
            )),
            "create_entity_missing_title",
        ),
        (
            "stub-body",
            "Stub Body.md",
            render_canonical_markdown(&CanonicalDocument::new(
                "title: Stub body\n",
                format!("{}\n", CanonicalDocument::STUB_MARKER),
            )),
            "create_entity_stub_body",
        ),
        (
            "directive",
            "Directive.md",
            render_canonical_markdown(&CanonicalDocument::new(
                "title: Directive\n",
                "::loc{id=seeded-block type=unsupported kind=\"unsupported\"}\n",
            )),
            "create_entity_directive_unsupported",
        ),
    ];

    for (name, relative_path, markdown, expected_code) in cases {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
            "block-1",
            "Create validation root.",
        )]));
        let connector = NotionConnector::with_api(
            NotionConfig::default().with_root_page_id(RemoteId::new("page-1")),
            api.clone(),
        );
        mount_virtual_workspace(&fixture, &mut store, "page-1");
        let content_root = fixture.content_root();
        hydrate_virtual_root_page(&fixture, &mut store, &connector, &content_root, "page-1");
        let created = create_virtual_fs_file(
            &mut store,
            &content_root,
            &fixture.mount_id,
            "children:page-1",
            relative_path,
        )
        .unwrap_or_else(|error| panic!("record create validation {name}: {error:?}"));
        commit_virtual_fs_write(
            &mut store,
            &content_root,
            &fixture.mount_id,
            &created.identifier,
            markdown.as_bytes(),
        )
        .unwrap_or_else(|error| panic!("write create validation {name}: {error:?}"));
        let page_path = fixture.root.clone();

        let diff = run_diff(&store, &page_path)
            .unwrap_or_else(|error| panic!("diff create validation {name}: {error:?}"));
        assert!(!diff.ok, "{name}: {diff:#?}");
        assert_eq!(diff.action, "fix_validation", "{name}: {diff:#?}");
        assert!(diff.plan.is_none(), "{name}: {diff:#?}");
        assert_eq!(diff.validation[0].code, expected_code, "{name}: {diff:#?}");

        let push = run_push_with_daemon_at_state_root(
            &mut store,
            &connector,
            &page_path,
            PushOptions {
                assume_yes: true,
                confirm_dangerous: false,
            },
            Some(&fixture.state_root),
        )
        .unwrap_or_else(|error| panic!("push create validation {name}: {error}"));
        assert!(!push.ok, "{name}: {push:#?}");
        assert_eq!(push.action, "fix_validation", "{name}: {push:#?}");
        assert!(push.plan.is_none(), "{name}: {push:#?}");
        assert_eq!(push.validation[0].code, expected_code, "{name}: {push:#?}");
        assert_eq!(push.push_id, None, "{name}: {push:#?}");
        assert_eq!(push.journal_status, None, "{name}: {push:#?}");
        assert!(
            store.list_journal().expect("journal").is_empty(),
            "{name}: create validation must block before journal creation"
        );
        let calls = api.calls.lock().expect("calls");
        assert!(
            calls.is_empty(),
            "{name}: create validation must block before connector apply: {calls:#?}"
        );
    }
}

#[test]
fn frontmatter_remote_id_mismatch_blocks_before_journaled_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
        "block-1",
        "Identity validation root.",
    )]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount frontmatter id mismatch fixture");

    let pull = run_pull(&mut store, &connector, &fixture.root).expect("pull identity page");
    assert!(pull.ok, "{pull:#?}");
    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read identity page");
    fs::write(&page_path, original.replace("id: page-1", "id: page-2"))
        .expect("write mismatched identity");

    let diff = run_diff(&store, &page_path).expect("diff mismatched identity");
    assert!(!diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(diff.validation[0].code, "frontmatter_remote_id_mismatch");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push mismatched identity");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert!(push.plan.is_none(), "{push:#?}");
    assert_eq!(push.validation[0].code, "frontmatter_remote_id_mismatch");
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(
        store.list_journal().expect("journal").is_empty(),
        "frontmatter id mismatch must block before journal creation"
    );
    let calls = api.calls.lock().expect("calls");
    assert!(
        calls.is_empty(),
        "frontmatter id mismatch must block before connector apply: {calls:#?}"
    );
}

#[test]
fn broker_oauth_connect_stores_refresh_handle_without_leaking_secret_material() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let credentials = FileCredentialStore::new(&fixture.state_root);
    let exchange = FakeBrokerOAuthExchange;

    let report = run_connect_notion_broker_oauth(
        &mut store,
        &credentials,
        BrokerOAuthConnectOptions {
            connection_id: Some(ConnectionId::new("broker-work")),
            broker_url: "https://auth.example.test".to_string(),
            client_id: "broker-client-id".to_string(),
            session: "broker-session".to_string(),
            state: "state-1".to_string(),
            code: "oauth-code".to_string(),
            redirect_uri: "http://localhost:8757/oauth/notion/callback".to_string(),
        },
        &exchange,
    )
    .expect("connect broker oauth");

    assert!(report.ok);
    assert_eq!(report.connection_id, "broker-work");
    assert_eq!(report.profile_id, DEFAULT_NOTION_OAUTH_PROFILE_ID);
    assert_eq!(report.auth_kind, "oauth");
    assert_eq!(report.workspace_name.as_deref(), Some("Locality"));

    let secret = credentials
        .get("connection:broker-work")
        .expect("broker oauth credential saved");
    let stored = serde_json::from_str::<StoredNotionCredential>(&secret).expect("stored oauth");
    assert_eq!(stored.kind, "oauth");
    assert_eq!(stored.access_token, "oauth-access-token");
    assert_eq!(stored.refresh_token, None);
    assert_eq!(
        stored.refresh_token_handle.as_deref(),
        Some("opaque-refresh-handle")
    );
    assert_eq!(stored.oauth_client_id.as_deref(), Some("broker-client-id"));
    assert_eq!(stored.oauth_client_secret, None);
    assert_eq!(
        stored.oauth_broker_url.as_deref(),
        Some("https://auth.example.test")
    );

    let connection = store
        .get_connection(&ConnectionId::new("broker-work"))
        .expect("load broker oauth connection")
        .expect("broker oauth connection");
    assert_eq!(connection.secret_ref, "connection:broker-work");
    assert_eq!(connection.auth_kind, "oauth");
    assert_eq!(connection.workspace_id.as_deref(), Some("workspace-1"));
    assert_eq!(
        connection.profile_id,
        Some(ConnectorProfileId::new(DEFAULT_NOTION_OAUTH_PROFILE_ID))
    );

    let profile = store
        .get_connector_profile(&ConnectorProfileId::new(DEFAULT_NOTION_OAUTH_PROFILE_ID))
        .expect("load oauth profile")
        .expect("oauth profile");
    assert_eq!(profile.connector, "notion");
    assert_eq!(profile.auth_kind, "oauth");

    let reports = [
        serde_json::to_string(&report).expect("connect report json"),
        serde_json::to_string(&run_connections(&store).expect("connections report"))
            .expect("connections json"),
        serde_json::to_string(
            &run_connection_show(&store, ConnectionId::new("broker-work"))
                .expect("connection show report"),
        )
        .expect("connection show json"),
        serde_json::to_string(&run_profiles(&store).expect("profiles report"))
            .expect("profiles json"),
    ];
    for json in reports {
        assert!(!json.contains("oauth-access-token"), "{json}");
        assert!(!json.contains("opaque-refresh-handle"), "{json}");
        assert!(!json.contains("broker-client-secret"), "{json}");
        assert!(!json.contains("secret_ref"), "{json}");
        assert!(!json.contains("connection:broker-work"), "{json}");
    }
}

#[test]
fn google_docs_broker_oauth_connect_stores_refresh_handle_without_leaking_secret_material() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let credentials = FileCredentialStore::new(&fixture.state_root);
    let exchange = FakeGoogleDocsBrokerOAuthExchange;

    let report = run_connect_google_docs_broker_oauth(
        &mut store,
        &credentials,
        GoogleDocsBrokerOAuthConnectOptions {
            connection_id: Some(ConnectionId::new("docs-broker-work")),
            broker_url: "https://auth.example.test".to_string(),
            client_id: "google-broker-client-id".to_string(),
            session: "google-broker-session".to_string(),
            state: "google-state-1".to_string(),
            code: "google-oauth-code".to_string(),
            redirect_uri: "http://localhost:8757/oauth/google-docs/callback".to_string(),
        },
        &exchange,
    )
    .expect("connect google docs broker oauth");

    assert!(report.ok);
    assert_eq!(report.connection_id, "docs-broker-work");
    assert_eq!(report.profile_id, DEFAULT_GOOGLE_DOCS_OAUTH_PROFILE_ID);
    assert_eq!(report.connector, "google-docs");
    assert_eq!(report.auth_kind, "oauth");
    assert_eq!(report.account_label.as_deref(), Some("user@example.com"));
    assert_eq!(report.workspace_name.as_deref(), Some("Google Drive"));

    let secret = credentials
        .get("connection:docs-broker-work")
        .expect("google docs broker oauth credential saved");
    let stored =
        serde_json::from_str::<StoredGoogleDocsCredential>(&secret).expect("stored google oauth");
    assert_eq!(stored.kind, "oauth");
    assert_eq!(stored.access_token, "google-oauth-access-token");
    assert_eq!(
        stored.refresh_token_handle.as_deref(),
        Some("google-opaque-refresh-handle")
    );
    assert_eq!(
        stored.oauth_client_id.as_deref(),
        Some("google-broker-client-id")
    );
    assert_eq!(
        stored.oauth_broker_url.as_deref(),
        Some("https://auth.example.test")
    );

    let connection = store
        .get_connection(&ConnectionId::new("docs-broker-work"))
        .expect("load google docs broker oauth connection")
        .expect("google docs broker oauth connection");
    assert_eq!(connection.secret_ref, "connection:docs-broker-work");
    assert_eq!(connection.connector, "google-docs");
    assert_eq!(connection.auth_kind, "oauth");
    assert_eq!(
        connection.account_label.as_deref(),
        Some("user@example.com")
    );
    assert_eq!(connection.workspace_id.as_deref(), Some("google-drive"));
    assert_eq!(
        connection.profile_id,
        Some(ConnectorProfileId::new(
            DEFAULT_GOOGLE_DOCS_OAUTH_PROFILE_ID
        ))
    );

    let profile = store
        .get_connector_profile(&ConnectorProfileId::new(
            DEFAULT_GOOGLE_DOCS_OAUTH_PROFILE_ID,
        ))
        .expect("load google docs oauth profile")
        .expect("google docs oauth profile");
    assert_eq!(profile.connector, "google-docs");
    assert_eq!(profile.auth_kind, "oauth");

    let reports = [
        serde_json::to_string(&report).expect("google docs connect report json"),
        serde_json::to_string(&run_connections(&store).expect("connections report"))
            .expect("connections json"),
        serde_json::to_string(
            &run_connection_show(&store, ConnectionId::new("docs-broker-work"))
                .expect("connection show report"),
        )
        .expect("connection show json"),
        serde_json::to_string(&run_profiles(&store).expect("profiles report"))
            .expect("profiles json"),
    ];
    for json in reports {
        assert!(!json.contains("google-oauth-access-token"), "{json}");
        assert!(!json.contains("google-opaque-refresh-handle"), "{json}");
        assert!(!json.contains("google-broker-client-secret"), "{json}");
        assert!(!json.contains("secret_ref"), "{json}");
        assert!(!json.contains("connection:docs-broker-work"), "{json}");
    }
}

#[test]
fn cli_mount_blocks_revoked_connection_before_creating_mount() {
    let fixture = E2eFixture::new();
    fs::create_dir_all(&fixture.root).expect("create mount root");
    let connection_id = ConnectionId::new("revoked-work");
    let profile_id = ConnectorProfileId::new(DEFAULT_NOTION_PROFILE_ID);
    let now = timestamp_string();
    let mut store =
        SqliteStateStore::open(fixture.state_root.clone()).expect("open revoked connection state");
    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: "notion".to_string(),
            display_name: "Notion token auth".to_string(),
            auth_kind: "token".to_string(),
            scopes: vec![],
            capabilities_json: notion_capabilities_json_for_live_test(),
            enabled_actions_json: "[\"read\",\"write\"]".to_string(),
            connector_version: "notion.v1".to_string(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
        })
        .expect("seed connector profile");
    store
        .save_connection(ConnectionRecord {
            connection_id: connection_id.clone(),
            profile_id: Some(profile_id),
            connector: "notion".to_string(),
            display_name: "Revoked Work".to_string(),
            account_label: Some("agent@example.com".to_string()),
            workspace_id: Some("workspace-1".to_string()),
            workspace_name: Some("Workspace".to_string()),
            auth_kind: "token".to_string(),
            secret_ref: format!("connection:{}", connection_id.as_str()),
            scopes: vec![],
            capabilities_json: notion_capabilities_json_for_live_test(),
            status: "revoked".to_string(),
            created_at: now.clone(),
            updated_at: now,
            expires_at: None,
        })
        .expect("seed revoked connection");
    drop(store);

    let loc = env!("CARGO_BIN_EXE_loc");
    let root = fixture.root.display().to_string();
    let mount = loc_json_with_exit(
        loc_command(loc, &fixture.state_root).args([
            "mount",
            "notion",
            root.as_str(),
            "--root-page",
            "page-1",
            "--connection",
            connection_id.as_str(),
            "--mount-id",
            fixture.mount_id.as_str(),
            "--json",
        ]),
        1,
    );

    assert_eq!(mount.value["ok"], false, "{mount:#?}");
    assert_eq!(mount.value["command"], "mount", "{mount:#?}");
    assert_eq!(mount.value["code"], "connection_revoked", "{mount:#?}");
    assert_eq!(
        mount.value["suggested_command"], "loc connect notion",
        "{mount:#?}"
    );
    assert!(
        !mount.stdout.contains("secret_ref"),
        "mount error should not expose credential storage internals"
    );
    let store =
        SqliteStateStore::open(fixture.state_root.clone()).expect("reopen revoked mount state");
    assert!(
        store
            .get_mount(&fixture.mount_id)
            .expect("query blocked mount")
            .is_none(),
        "revoked connection must block before writing mount state"
    );
}

#[test]
fn cli_mount_blocks_missing_connection_credential_before_creating_mount() {
    let seeded = seed_missing_credential_notion_connection("mount-missing-credential-work");

    let loc = env!("CARGO_BIN_EXE_loc");
    let root = seeded.fixture.root.display().to_string();
    let mount = loc_json_with_exit(
        loc_command(loc, &seeded.fixture.state_root).args([
            "mount",
            "notion",
            root.as_str(),
            "--root-page",
            "page-1",
            "--connection",
            seeded.connection_id.as_str(),
            "--mount-id",
            seeded.fixture.mount_id.as_str(),
            "--json",
        ]),
        1,
    );

    assert_eq!(mount.value["ok"], false, "{mount:#?}");
    assert_eq!(mount.value["command"], "mount", "{mount:#?}");
    assert_eq!(mount.value["code"], "auth_required", "{mount:#?}");
    assert_eq!(
        mount.value["suggested_command"], "loc connect notion",
        "{mount:#?}"
    );
    assert!(
        !mount.stdout.contains("secret_ref"),
        "mount error should not expose credential storage internals"
    );
    assert!(
        !mount.stdout.contains(&seeded.secret_ref),
        "mount error leaked credential storage internals"
    );
    let store =
        SqliteStateStore::open(seeded.fixture.state_root.clone()).expect("reopen mount state");
    assert!(
        store
            .get_mount(&seeded.fixture.mount_id)
            .expect("query blocked mount")
            .is_none(),
        "missing credential must block before writing mount state"
    );
}

#[test]
fn cli_doctor_reports_missing_mount_credential_without_leaking_secret_ref() {
    let seeded = seed_missing_credential_notion_mount("missing-credential-work");

    let loc = env!("CARGO_BIN_EXE_loc");
    let doctor = loc_json_with_exit(
        loc_command(loc, &seeded.fixture.state_root).args(["doctor", "--json"]),
        3,
    );

    assert_eq!(doctor.value["ok"], false, "{doctor:#?}");
    assert_eq!(doctor.value["command"], "doctor", "{doctor:#?}");
    assert_eq!(doctor.value["status"], "error", "{doctor:#?}");
    let connection = doctor.value["connections"]
        .as_array()
        .expect("doctor connections")
        .iter()
        .find(|connection| connection["connection_id"] == seeded.connection_id.as_str())
        .expect("missing credential connection");
    assert_eq!(connection["status"], "active", "{doctor:#?}");
    assert_eq!(connection["profile_status"], "ok", "{doctor:#?}");
    assert_eq!(connection["credential_status"], "missing", "{doctor:#?}");
    let finding = doctor.value["findings"]
        .as_array()
        .expect("doctor findings")
        .iter()
        .find(|finding| finding["code"] == "connection_credential_missing")
        .expect("missing credential finding");
    assert_eq!(
        finding["connection_id"],
        seeded.connection_id.as_str(),
        "{doctor:#?}"
    );
    assert_eq!(
        finding["suggested_command"], "loc connect notion",
        "{doctor:#?}"
    );
    assert!(
        doctor.value["suggested_commands"]
            .as_array()
            .expect("doctor suggested commands")
            .iter()
            .any(|command| command == "loc connect notion"),
        "{doctor:#?}"
    );
    assert!(
        !doctor.stdout.contains(&seeded.secret_ref),
        "doctor JSON leaked credential storage internals"
    );
}

#[test]
fn cli_pull_missing_mount_credential_blocks_before_writing_files() {
    let seeded = seed_missing_credential_notion_mount("pull-missing-credential-work");
    let sentinel = seeded.fixture.root.join("local-note.txt");
    fs::write(&sentinel, "keep me\n").expect("write local sentinel");

    let loc = env!("CARGO_BIN_EXE_loc");
    let root = seeded.fixture.root.display().to_string();
    let pull = loc_json_with_exit(
        loc_command(loc, &seeded.fixture.state_root).args(["pull", root.as_str(), "--json"]),
        1,
    );

    assert_eq!(pull.value["ok"], false, "{pull:#?}");
    assert_eq!(pull.value["command"], "pull", "{pull:#?}");
    assert_eq!(pull.value["code"], "auth_required", "{pull:#?}");
    assert_eq!(
        pull.value["suggested_command"], "loc connect notion",
        "{pull:#?}"
    );
    assert!(
        !pull.stdout.contains("secret_ref"),
        "pull error should not expose credential storage internals"
    );
    assert!(
        !pull.stdout.contains(&seeded.secret_ref),
        "pull error leaked credential storage internals"
    );
    assert_eq!(
        fs::read_to_string(&sentinel).expect("read local sentinel"),
        "keep me\n",
        "credential failure must not rewrite existing local files"
    );
    let files = collect_files(&seeded.fixture.root);
    assert_eq!(
        files,
        vec![sentinel],
        "credential failure must not materialize remote files"
    );
}

#[test]
fn cli_push_missing_mount_credential_blocks_before_journal() {
    let seeded = seed_missing_credential_notion_mount("push-missing-credential-work");
    let page_path = seed_missing_credential_dirty_page(&seeded);

    let loc = env!("CARGO_BIN_EXE_loc");
    let page = page_path.display().to_string();
    let push = loc_json_with_exit(
        loc_command(loc, &seeded.fixture.state_root).args([
            "push",
            page.as_str(),
            "--yes",
            "--json",
        ]),
        1,
    );

    assert_eq!(push.value["ok"], false, "{push:#?}");
    assert_eq!(push.value["command"], "push", "{push:#?}");
    assert_eq!(push.value["code"], "auth_required", "{push:#?}");
    assert_eq!(
        push.value["suggested_command"], "loc connect notion",
        "{push:#?}"
    );
    assert!(
        !push.stdout.contains("secret_ref"),
        "push error should not expose credential storage internals"
    );
    assert!(
        !push.stdout.contains(&seeded.secret_ref),
        "push error leaked credential storage internals"
    );
    assert!(
        fs::read_to_string(&page_path)
            .expect("read dirty page after blocked push")
            .contains("Edited while credential is missing."),
        "blocked push must preserve the local edit"
    );
    let store = SqliteStateStore::open(seeded.fixture.state_root.clone())
        .expect("reopen missing credential push state");
    assert!(
        store.list_journal().expect("list journals").is_empty(),
        "missing credential must block before creating a push journal"
    );
}

#[test]
fn cli_inspect_missing_mount_credential_reports_reconnect_without_mutating() {
    let seeded = seed_missing_credential_notion_mount("inspect-missing-credential-work");
    let page_path = seed_missing_credential_dirty_page(&seeded);
    let before = fs::read_to_string(&page_path).expect("read page before blocked inspect");

    let loc = env!("CARGO_BIN_EXE_loc");
    let page = page_path.display().to_string();
    let inspect = loc_json_with_exit(
        loc_command(loc, &seeded.fixture.state_root).args(["inspect", page.as_str(), "--json"]),
        1,
    );

    assert_eq!(inspect.value["ok"], false, "{inspect:#?}");
    assert_eq!(inspect.value["command"], "inspect", "{inspect:#?}");
    assert_eq!(inspect.value["code"], "auth_required", "{inspect:#?}");
    assert_eq!(
        inspect.value["suggested_command"], "loc connect notion",
        "{inspect:#?}"
    );
    assert!(
        !inspect.stdout.contains("secret_ref"),
        "inspect error should not expose credential storage internals"
    );
    assert!(
        !inspect.stdout.contains(&seeded.secret_ref),
        "inspect error leaked credential storage internals"
    );
    assert_eq!(
        fs::read_to_string(&page_path).expect("read page after blocked inspect"),
        before,
        "blocked inspect must not rewrite the local file"
    );
    let store = SqliteStateStore::open(seeded.fixture.state_root.clone())
        .expect("reopen missing credential inspect state");
    assert!(
        store.list_journal().expect("list journals").is_empty(),
        "inspect must stay read-only when credential resolution fails"
    );
}

#[test]
fn cli_status_and_info_missing_mount_credential_stay_local_without_leaking_secret_ref() {
    let seeded = seed_missing_credential_notion_mount("status-info-missing-credential-work");
    let page_path = seed_missing_credential_dirty_page(&seeded);

    let loc = env!("CARGO_BIN_EXE_loc");
    let page = page_path.display().to_string();
    let status = loc_json_ok(loc_command(loc, &seeded.fixture.state_root).args([
        "status",
        page.as_str(),
        "--json",
    ]));

    assert_eq!(status.value["ok"], true, "{status:#?}");
    assert_eq!(status.value["command"], "status", "{status:#?}");
    assert_eq!(status.value["clean"], false, "{status:#?}");
    assert_eq!(status.value["summary"]["dirty"], 1, "{status:#?}");
    assert_eq!(
        status.value["mounts"][0]["mount_id"],
        seeded.fixture.mount_id.as_str(),
        "{status:#?}"
    );
    let entry = &status.value["mounts"][0]["entries"][0];
    assert_eq!(entry["entity_id"], "page-1", "{status:#?}");
    assert_eq!(entry["state"], "dirty", "{status:#?}");
    assert_eq!(entry["sync_state"], "pending_local_changes", "{status:#?}");
    assert!(
        !status.stdout.contains("secret_ref"),
        "status report should not expose credential storage internals"
    );
    assert!(
        !status.stdout.contains(&seeded.secret_ref),
        "status report leaked credential storage internals"
    );

    let info = loc_json_ok(loc_command(loc, &seeded.fixture.state_root).args([
        "info",
        page.as_str(),
        "--json",
    ]));

    assert_eq!(info.value["ok"], true, "{info:#?}");
    assert_eq!(info.value["command"], "info", "{info:#?}");
    assert_eq!(
        info.value["mount"]["mount_id"],
        seeded.fixture.mount_id.as_str(),
        "{info:#?}"
    );
    assert_eq!(info.value["mount"]["connector"], "notion", "{info:#?}");
    assert_eq!(info.value["subject"]["role"], "page_file", "{info:#?}");
    assert_eq!(info.value["subject"]["source"], "Notion page", "{info:#?}");
    assert_eq!(
        info.value["subject"]["entity"]["entity_id"], "page-1",
        "{info:#?}"
    );
    assert!(
        !info.stdout.contains("secret_ref"),
        "info report should not expose credential storage internals"
    );
    assert!(
        !info.stdout.contains(&seeded.secret_ref),
        "info report leaked credential storage internals"
    );
}

#[test]
fn cli_search_missing_mount_credential_returns_local_index_without_leaking_secret_ref() {
    let seeded = seed_missing_credential_notion_mount("search-missing-credential-work");
    let page_path = seed_missing_credential_dirty_page(&seeded);

    let loc = env!("CARGO_BIN_EXE_loc");
    let search = loc_json_ok(loc_command(loc, &seeded.fixture.state_root).args([
        "search",
        "Missing Credential Page",
        "--connector",
        "notion",
        "--json",
    ]));

    assert_eq!(search.value["ok"], true, "{search:#?}");
    assert_eq!(search.value["command"], "search", "{search:#?}");
    assert_eq!(
        search.value["query"], "Missing Credential Page",
        "{search:#?}"
    );
    assert_eq!(search.value["connector"], "notion", "{search:#?}");
    let result = search.value["results"]
        .as_array()
        .expect("search results")
        .iter()
        .find(|result| result["remote_id"] == "page-1")
        .expect("local indexed page search result");
    assert_eq!(result["mount_id"], seeded.fixture.mount_id.as_str());
    assert_eq!(result["connector"], "notion");
    assert_eq!(result["title"], "Missing Credential Page");
    assert_eq!(result["kind"], "page");
    assert_eq!(result["path"], "Missing Credential Page/page.md");
    assert_eq!(
        result["absolute_path"],
        page_path.display().to_string(),
        "{search:#?}"
    );
    assert!(
        !search.stdout.contains("secret_ref"),
        "search report should not expose credential storage internals"
    );
    assert!(
        !search.stdout.contains(&seeded.secret_ref),
        "search report leaked credential storage internals"
    );
}

#[test]
fn cli_diff_missing_mount_credential_plans_from_local_shadow() {
    let seeded = seed_missing_credential_notion_mount("diff-missing-credential-work");
    let page_path = seed_missing_credential_dirty_page(&seeded);
    let before = fs::read_to_string(&page_path).expect("read page before diff");

    let loc = env!("CARGO_BIN_EXE_loc");
    let page = page_path.display().to_string();
    let diff = loc_json_ok(loc_command(loc, &seeded.fixture.state_root).args([
        "diff",
        page.as_str(),
        "--json",
    ]));

    assert_eq!(diff.value["ok"], true, "{diff:#?}");
    assert_eq!(diff.value["command"], "diff", "{diff:#?}");
    assert_eq!(diff.value["action"], "confirm_plan", "{diff:#?}");
    assert_eq!(diff.value["mount_id"], seeded.fixture.mount_id.as_str());
    assert_eq!(diff.value["entity_id"], "page-1", "{diff:#?}");
    let plan = &diff.value["plan"];
    assert_eq!(plan["summary"]["blocks_updated"], 1, "{diff:#?}");
    assert_eq!(plan["operations"][0]["type"], "update_block", "{diff:#?}");
    assert!(
        !diff.stdout.contains("secret_ref"),
        "diff report should not expose credential storage internals"
    );
    assert!(
        !diff.stdout.contains(&seeded.secret_ref),
        "diff report leaked credential storage internals"
    );
    assert_eq!(
        fs::read_to_string(&page_path).expect("read page after diff"),
        before,
        "diff must not rewrite local files"
    );
    let store = SqliteStateStore::open(seeded.fixture.state_root.clone())
        .expect("reopen missing credential diff state");
    assert!(
        store.list_journal().expect("list journals").is_empty(),
        "diff must not create a journal when planning offline"
    );
}

#[test]
fn cli_restore_missing_mount_credential_restores_from_local_shadow() {
    let seeded = seed_missing_credential_notion_mount("restore-missing-credential-work");
    let page_path = seed_missing_credential_dirty_page(&seeded);
    assert!(
        fs::read_to_string(&page_path)
            .expect("read dirty page before restore")
            .contains("Edited while credential is missing."),
        "fixture should start dirty"
    );

    let loc = env!("CARGO_BIN_EXE_loc");
    let page = page_path.display().to_string();
    let restore = loc_json_with_exit(
        loc_command(loc, &seeded.fixture.state_root).args(["restore", page.as_str(), "--json"]),
        0,
    );

    assert_eq!(restore.value["ok"], true, "{restore:#?}");
    assert_eq!(restore.value["command"], "restore", "{restore:#?}");
    assert_eq!(restore.value["action"], "restored", "{restore:#?}");
    assert_eq!(restore.value["mount_id"], seeded.fixture.mount_id.as_str());
    assert_eq!(restore.value["entity_id"], "page-1", "{restore:#?}");
    assert!(
        !restore.stdout.contains("secret_ref"),
        "restore report should not expose credential storage internals"
    );
    assert!(
        !restore.stdout.contains(&seeded.secret_ref),
        "restore report leaked credential storage internals"
    );

    let restored = fs::read_to_string(&page_path).expect("read restored page");
    assert!(
        restored.contains("Synced body before credential loss."),
        "{restored}"
    );
    assert!(
        !restored.contains("Edited while credential is missing."),
        "{restored}"
    );

    let status = loc_json_ok(loc_command(loc, &seeded.fixture.state_root).args([
        "status",
        page.as_str(),
        "--json",
    ]));
    assert_eq!(status.value["clean"], true, "{status:#?}");
    assert_eq!(status.value["summary"]["dirty"], 0, "{status:#?}");
}

#[test]
fn cli_log_and_undo_prepared_journal_missing_credential_stay_local_without_secret_ref() {
    let seeded = seed_missing_credential_notion_mount("history-missing-credential-work");
    let page_path = seed_missing_credential_dirty_page(&seeded);
    let push_id = PushId("prepared-missing-credential-push".to_string());
    let shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        "Synced body before credential loss.",
        10,
        [RemoteId::new("block-1")],
    )
    .expect("prepared journal preimage shadow");
    let mut store = SqliteStateStore::open(seeded.fixture.state_root.clone())
        .expect("open missing credential history state");
    store
        .append_journal(
            JournalEntry::new(
                push_id.clone(),
                seeded.fixture.mount_id.clone(),
                vec![RemoteId::new("page-1")],
                PushPlan::new(
                    vec![RemoteId::new("page-1")],
                    vec![PushOperation::UpdateBlock {
                        block_id: RemoteId::new("block-1"),
                        content: "Prepared local journal update.".to_string(),
                    }],
                ),
                JournalStatus::Prepared,
            )
            .with_preimages(vec![JournalPreimage::from_shadow(shadow)]),
        )
        .expect("seed prepared journal");
    drop(store);

    let loc = env!("CARGO_BIN_EXE_loc");
    let page = page_path.display().to_string();
    let log = loc_json_ok(loc_command(loc, &seeded.fixture.state_root).args([
        "log",
        page.as_str(),
        "--json",
    ]));
    assert_eq!(log.value["ok"], true, "{log:#?}");
    assert_eq!(log.value["command"], "log", "{log:#?}");
    let entries = log.value["entries"].as_array().expect("log entries");
    assert_eq!(entries.len(), 1, "{log:#?}");
    assert_eq!(entries[0]["push_id"], push_id.0.as_str(), "{log:#?}");
    assert_eq!(entries[0]["status"], "prepared", "{log:#?}");
    assert_eq!(entries[0]["operation_count"], 1, "{log:#?}");
    assert_eq!(entries[0]["plan_summary"]["blocks_updated"], 1, "{log:#?}");
    assert!(
        !log.stdout.contains("secret_ref"),
        "log report should not expose credential storage internals"
    );
    assert!(
        !log.stdout.contains(&seeded.secret_ref),
        "log report leaked credential storage internals"
    );

    let undo = loc_json_ok(loc_command(loc, &seeded.fixture.state_root).args([
        "undo",
        push_id.0.as_str(),
        "--json",
    ]));
    assert_eq!(undo.value["ok"], true, "{undo:#?}");
    assert_eq!(undo.value["command"], "undo", "{undo:#?}");
    assert_eq!(undo.value["action"], "reverted_local_journal", "{undo:#?}");
    assert_eq!(undo.value["status"], "reverted", "{undo:#?}");
    assert_eq!(undo.value["entry"]["status"], "reverted", "{undo:#?}");
    assert!(
        !undo.stdout.contains("secret_ref"),
        "undo report should not expose credential storage internals"
    );
    assert!(
        !undo.stdout.contains(&seeded.secret_ref),
        "undo report leaked credential storage internals"
    );

    let store = SqliteStateStore::open(seeded.fixture.state_root.clone())
        .expect("reopen missing credential history state");
    assert_eq!(
        store
            .get_journal(&push_id)
            .expect("get prepared journal after undo")
            .expect("prepared journal after undo")
            .status,
        JournalStatus::Reverted
    );
}

#[test]
fn cli_connection_reports_and_disconnect_missing_credential_without_leaking_secret_ref() {
    let seeded = seed_missing_credential_notion_connection("disconnect-missing-credential-work");

    let loc = env!("CARGO_BIN_EXE_loc");
    let connections =
        loc_json_ok(loc_command(loc, &seeded.fixture.state_root).args(["connections", "--json"]));
    assert_eq!(connections.value["ok"], true, "{connections:#?}");
    let connection = connections.value["connections"]
        .as_array()
        .expect("connections")
        .iter()
        .find(|connection| connection["connection_id"] == seeded.connection_id.as_str())
        .expect("seeded missing-credential connection");
    assert_eq!(connection["status"], "active", "{connections:#?}");
    assert_eq!(connection["connector"], "notion", "{connections:#?}");
    assert!(
        !connections.stdout.contains("secret_ref"),
        "connections report should not expose credential storage internals"
    );
    assert!(
        !connections.stdout.contains(&seeded.secret_ref),
        "connections report leaked credential storage internals"
    );

    let active_show = loc_json_ok(loc_command(loc, &seeded.fixture.state_root).args([
        "connection",
        "show",
        seeded.connection_id.as_str(),
        "--json",
    ]));
    assert_eq!(
        active_show.value["connection"]["connection_id"],
        seeded.connection_id.as_str(),
        "{active_show:#?}"
    );
    assert_eq!(
        active_show.value["connection"]["status"], "active",
        "{active_show:#?}"
    );
    assert!(
        !active_show.stdout.contains("secret_ref"),
        "connection show should not expose credential storage internals"
    );
    assert!(
        !active_show.stdout.contains(&seeded.secret_ref),
        "connection show leaked credential storage internals"
    );

    let disconnect = loc_json_ok(loc_command(loc, &seeded.fixture.state_root).args([
        "disconnect",
        seeded.connection_id.as_str(),
        "--json",
    ]));
    assert_eq!(disconnect.value["ok"], true, "{disconnect:#?}");
    assert_eq!(disconnect.value["command"], "disconnect", "{disconnect:#?}");
    assert_eq!(
        disconnect.value["connection_id"],
        seeded.connection_id.as_str(),
        "{disconnect:#?}"
    );
    assert_eq!(disconnect.value["status"], "revoked", "{disconnect:#?}");
    assert!(
        !disconnect.stdout.contains("secret_ref"),
        "disconnect report should not expose credential storage internals"
    );
    assert!(
        !disconnect.stdout.contains(&seeded.secret_ref),
        "disconnect report leaked credential storage internals"
    );
    assert!(
        FileCredentialStore::new(&seeded.fixture.state_root)
            .get(&seeded.secret_ref)
            .is_err(),
        "disconnect must not recreate a missing credential"
    );

    let revoked_show = loc_json_ok(loc_command(loc, &seeded.fixture.state_root).args([
        "connection",
        "show",
        seeded.connection_id.as_str(),
        "--json",
    ]));
    assert_eq!(
        revoked_show.value["connection"]["status"], "revoked",
        "{revoked_show:#?}"
    );
    assert!(
        !revoked_show.stdout.contains("secret_ref"),
        "revoked connection show should not expose credential storage internals"
    );
    assert!(
        !revoked_show.stdout.contains(&seeded.secret_ref),
        "revoked connection show leaked credential storage internals"
    );
}

#[test]
fn cli_reset_requires_yes_and_clears_isolated_state_without_deleting_visible_files() {
    let fixture = E2eFixture::new();
    fs::create_dir_all(&fixture.root).expect("create visible mount root");
    fs::write(fixture.root.join("page.md"), b"user-visible content").expect("write visible file");
    fs::create_dir_all(fixture.state_root.join("content/notion-main")).expect("create content");
    fs::write(
        fixture.state_root.join("content/notion-main/page.md"),
        b"cached",
    )
    .expect("write cached content");

    let mut store =
        SqliteStateStore::open(fixture.state_root.clone()).expect("open reset test state");
    store
        .save_connection(ConnectionRecord {
            connection_id: ConnectionId::new("reset-work"),
            profile_id: None,
            connector: "notion".to_string(),
            display_name: "Reset Work".to_string(),
            account_label: None,
            workspace_id: None,
            workspace_name: None,
            auth_kind: "token".to_string(),
            secret_ref: "connection:reset-work".to_string(),
            scopes: vec![],
            capabilities_json: "{}".to_string(),
            status: "active".to_string(),
            created_at: timestamp_string(),
            updated_at: timestamp_string(),
            expires_at: None,
        })
        .expect("save reset connection");
    drop(store);

    let credentials = FileCredentialStore::new(&fixture.state_root);
    credentials
        .put("connection:reset-work", "reset-secret")
        .expect("write reset credential");

    let loc = env!("CARGO_BIN_EXE_loc");
    let rejected = loc_json_with_exit(
        loc_command(loc, &fixture.state_root).args(["reset", "--json"]),
        2,
    );
    assert_eq!(rejected.value["ok"], false, "{rejected:#?}");
    assert_eq!(rejected.value["command"], "reset", "{rejected:#?}");
    assert_eq!(
        rejected.value["code"], "confirmation_required",
        "{rejected:#?}"
    );
    assert!(fixture.state_root.join("state.sqlite3").exists());
    assert_eq!(
        credentials
            .get("connection:reset-work")
            .expect("credential should remain before confirmed reset"),
        "reset-secret"
    );

    let reset =
        loc_json_ok(loc_command(loc, &fixture.state_root).args(["reset", "--yes", "--json"]));
    assert_eq!(reset.value["ok"], true, "{reset:#?}");
    assert_eq!(reset.value["command"], "reset", "{reset:#?}");
    assert_eq!(reset.value["action"], "reset", "{reset:#?}");
    assert_eq!(
        reset.value["state_root"],
        fixture.state_root.display().to_string(),
        "{reset:#?}"
    );
    assert!(
        reset.value["deleted_credentials"].as_u64().expect("count") >= 1,
        "{reset:#?}"
    );
    assert!(
        !reset.stdout.contains("connection:reset-work"),
        "reset report should not expose credential storage refs"
    );
    assert!(
        !reset.stdout.contains("reset-secret"),
        "reset report should not expose credential values"
    );
    assert!(
        fs::read_dir(&fixture.state_root)
            .expect("read reset state root")
            .next()
            .is_none(),
        "confirmed reset should clear state root"
    );
    assert_eq!(
        fs::read(fixture.root.join("page.md")).expect("read visible file"),
        b"user-visible content"
    );
    assert!(
        credentials.get("connection:reset-work").is_err(),
        "confirmed reset should delete connection credential"
    );
}

struct MissingCredentialMount {
    fixture: E2eFixture,
    connection_id: ConnectionId,
    secret_ref: String,
}

fn seed_missing_credential_notion_mount(connection_id: &str) -> MissingCredentialMount {
    let seeded = seed_missing_credential_notion_connection(connection_id);
    let mut store = SqliteStateStore::open(seeded.fixture.state_root.clone())
        .expect("open missing credential mounted state");
    store
        .save_mount(
            MountConfig::new(
                seeded.fixture.mount_id.clone(),
                "notion",
                seeded.fixture.root.clone(),
            )
            .with_remote_root_id(RemoteId::new("page-1"))
            .with_connection_id(seeded.connection_id.clone())
            .projection(ProjectionMode::PlainFiles),
        )
        .expect("seed mounted workspace");
    drop(store);
    seeded
}

fn seed_missing_credential_notion_connection(connection_id: &str) -> MissingCredentialMount {
    let fixture = E2eFixture::new();
    fs::create_dir_all(&fixture.root).expect("create mounted root");
    let connection_id = ConnectionId::new(connection_id);
    let profile_id = ConnectorProfileId::new(DEFAULT_NOTION_PROFILE_ID);
    let secret_ref = format!("connection:{}", connection_id.as_str());
    let now = timestamp_string();
    let mut store =
        SqliteStateStore::open(fixture.state_root.clone()).expect("open missing credential state");
    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: "notion".to_string(),
            display_name: "Notion token auth".to_string(),
            auth_kind: "token".to_string(),
            scopes: vec![],
            capabilities_json: notion_capabilities_json_for_live_test(),
            enabled_actions_json: "[\"read\",\"write\"]".to_string(),
            connector_version: "notion.v1".to_string(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
        })
        .expect("seed connector profile");
    store
        .save_connection(ConnectionRecord {
            connection_id: connection_id.clone(),
            profile_id: Some(profile_id),
            connector: "notion".to_string(),
            display_name: "Missing Credential Work".to_string(),
            account_label: Some("agent@example.com".to_string()),
            workspace_id: Some("workspace-1".to_string()),
            workspace_name: Some("Workspace".to_string()),
            auth_kind: "token".to_string(),
            secret_ref: secret_ref.clone(),
            scopes: vec![],
            capabilities_json: notion_capabilities_json_for_live_test(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            expires_at: None,
        })
        .expect("seed active connection without credential");
    drop(store);
    MissingCredentialMount {
        fixture,
        connection_id,
        secret_ref,
    }
}

fn seed_missing_credential_dirty_page(seeded: &MissingCredentialMount) -> PathBuf {
    let remote_id = RemoteId::new("page-1");
    let title = "Missing Credential Page";
    let relative_path = "Missing Credential Page/page.md";
    let page_path =
        locality_platform::join_logical_path(&seeded.fixture.root, Path::new(relative_path));
    let frontmatter = "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Missing Credential Page\n";
    let synced_body = "Synced body before credential loss.";
    let dirty_body = "Edited while credential is missing.";
    let mut store = SqliteStateStore::open(seeded.fixture.state_root.clone())
        .expect("open missing credential dirty page state");
    store
        .save_entity(
            EntityRecord::new(
                seeded.fixture.mount_id.clone(),
                remote_id.clone(),
                EntityKind::Page,
                title,
                relative_path,
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save dirty page entity");
    let shadow =
        ShadowDocument::from_synced_body(remote_id, synced_body, 10, [RemoteId::new("block-1")])
            .expect("dirty page shadow")
            .with_frontmatter(frontmatter);
    store
        .save_shadow(&seeded.fixture.mount_id, shadow)
        .expect("save dirty page shadow");
    drop(store);

    fs::create_dir_all(page_path.parent().expect("dirty page parent"))
        .expect("create dirty page parent");
    fs::write(
        &page_path,
        render_canonical_markdown(&CanonicalDocument::new(frontmatter, dirty_body)),
    )
    .expect("write dirty page");
    page_path
}

#[test]
fn google_docs_workspace_folder_pull_stubs_nested_docs_without_hydrating_folder() {
    let fixture = E2eFixture::new();
    let mount_id = MountId::new("google-docs-main");
    let mut store = InMemoryStateStore::new();
    let drive = Arc::new(
        FakeGoogleDrive::default()
            .with_children(
                "workspace-folder",
                vec![google_drive_folder(
                    "folder-1",
                    "Marketing",
                    "workspace-folder",
                )],
            )
            .with_children(
                "folder-1",
                vec![google_drive_doc("doc-1", "Launch Brief", "folder-1")],
            ),
    );
    let docs = Arc::new(FakeGoogleDocs::default().with_document(google_document(
        "doc-1",
        "Launch Brief",
        "rev-1",
        "Launch body.\n",
    )));
    let connector = GoogleDocsConnector::with_apis(
        GoogleDocsConfig::new("token").with_workspace_folder_id(RemoteId::new("workspace-folder")),
        drive,
        docs.clone(),
    );

    run_mount(
        &mut store,
        MountOptions {
            mount_id: mount_id.clone(),
            connector: "google-docs".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("workspace-folder")),
            connection_id: Some(ConnectionId::new("google-docs-work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount Google Docs workspace folder");

    let pull =
        run_pull(&mut store, &connector, &fixture.root).expect("pull Google Docs workspace folder");

    assert!(pull.ok, "{pull:#?}");
    assert_eq!(pull.enumerated, 2, "{pull:#?}");
    assert_eq!(pull.stubbed, 1, "{pull:#?}");
    assert_eq!(
        pull.hydrated, 0,
        "workspace folder pulls must not try to hydrate folder entries as docs: {pull:#?}"
    );
    assert_eq!(
        docs.get_count(),
        0,
        "pulling a Google Drive workspace folder should leave nested docs online-only"
    );

    let folder_path = fixture.root.join("marketing");
    let page_path = fixture.root.join("marketing/launch-brief/page.md");
    assert!(folder_path.is_dir(), "folder should be projected");
    let stub = fs::read_to_string(&page_path).expect("read Google Docs stub");
    assert!(stub.contains("loc:\n  id: doc-1"), "{stub}");
    assert!(stub.contains("title: Launch Brief"), "{stub}");
    assert!(
        !stub.contains("Launch body."),
        "root workspace pull should create an online-only stub, not hydrate the doc body: {stub}"
    );
}

#[test]
fn google_docs_mount_pull_edit_push_reconciles_clean_with_real_connector() {
    let fixture = E2eFixture::new();
    let mount_id = MountId::new("google-docs-main");
    let mut store = InMemoryStateStore::new();
    let drive = Arc::new(FakeGoogleDrive::default().with_children(
        "workspace-folder",
        vec![google_drive_doc(
            "doc-1",
            "Launch Brief",
            "workspace-folder",
        )],
    ));
    let docs = Arc::new(FakeGoogleDocs::default().with_document(google_document(
        "doc-1",
        "Launch Brief",
        "rev-1",
        "Original line.\n",
    )));
    let connector = GoogleDocsConnector::with_apis(
        GoogleDocsConfig::new("token").with_workspace_folder_id(RemoteId::new("workspace-folder")),
        drive,
        docs.clone(),
    );

    run_mount(
        &mut store,
        MountOptions {
            mount_id: mount_id.clone(),
            connector: "google-docs".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("workspace-folder")),
            connection_id: Some(ConnectionId::new("google-docs-work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount Google Docs edit workflow");

    let root_pull =
        run_pull(&mut store, &connector, &fixture.root).expect("pull Google Docs workspace root");
    assert_eq!(root_pull.stubbed, 1, "{root_pull:#?}");
    assert_eq!(root_pull.hydrated, 0, "{root_pull:#?}");

    let page_path = fixture.root.join("launch-brief/page.md");
    let page_pull = run_pull(&mut store, &connector, &page_path).expect("hydrate Google Doc");
    assert_eq!(page_pull.hydrated, 1, "{page_pull:#?}");

    let original = fs::read_to_string(&page_path).expect("read hydrated Google Doc");
    assert!(original.contains("Original line."), "{original}");
    fs::write(
        &page_path,
        original.replace("Original line.", "Updated line."),
    )
    .expect("write Google Docs local edit");

    let diff = run_diff(&store, &page_path).expect("diff Google Docs edit");
    assert!(diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("Google Docs edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{diff:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push Google Docs edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.changed_remote_ids, vec!["doc-1"], "{push:#?}");
    assert_eq!(
        docs.batch_count(),
        1,
        "Google Docs API should receive one batch update"
    );

    let reconciled = fs::read_to_string(&page_path).expect("read reconciled Google Doc");
    assert!(reconciled.contains("Updated line."), "{reconciled}");
    assert!(!reconciled.contains("Original line."), "{reconciled}");

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status after Google Docs edit push");
    assert!(status.clean, "{status:#?}");
    assert_eq!(status.summary.dirty, 0, "{status:#?}");
}

#[test]
fn google_docs_mount_create_push_reconciles_clean_with_real_connector() {
    let fixture = E2eFixture::new();
    let mount_id = MountId::new("google-docs-main");
    let mut store = InMemoryStateStore::new();
    let drive = Arc::new(FakeGoogleDrive::default());
    let docs = Arc::new(FakeGoogleDocs::default());
    let connector = GoogleDocsConnector::with_apis(
        GoogleDocsConfig::new("token").with_workspace_folder_id(RemoteId::new("workspace-folder")),
        drive.clone(),
        docs.clone(),
    );

    run_mount(
        &mut store,
        MountOptions {
            mount_id: mount_id.clone(),
            connector: "google-docs".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("workspace-folder")),
            connection_id: Some(ConnectionId::new("google-docs-work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount Google Docs create workflow");

    let root_pull =
        run_pull(&mut store, &connector, &fixture.root).expect("pull empty Google Docs workspace");
    assert!(root_pull.ok, "{root_pull:#?}");
    assert_eq!(root_pull.enumerated, 0, "{root_pull:#?}");

    let page_dir = fixture.root.join("draft-plan");
    fs::create_dir_all(&page_dir).expect("create Google Docs local page dir");
    let page_path = page_dir.join("page.md");
    fs::write(
        &page_path,
        "---\ntitle: Draft Plan\n---\n# Draft Plan\n\nCreated from filesystem.\n",
    )
    .expect("write Google Docs local create");

    let diff = run_diff(&store, &page_path).expect("diff Google Docs create");
    assert!(diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("Google Docs create plan");
    assert_eq!(plan.summary.entities_created, 1, "{plan:#?}");
    assert_eq!(
        plan.affected_entities,
        vec!["workspace-folder"],
        "{plan:#?}"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push Google Docs create");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.changed_remote_ids, vec!["created-doc-1"], "{push:#?}");
    assert_eq!(
        docs.batch_count(),
        1,
        "Google Docs API should receive one batch update for the new document body"
    );

    let created_file = drive
        .get_file("created-doc-1")
        .expect("created Google Docs Drive file");
    assert_eq!(created_file.name, "Draft Plan");
    assert_eq!(created_file.parents, vec!["workspace-folder"]);
    let workspace_children = drive
        .list_children("workspace-folder", None)
        .expect("workspace children after Google Docs create");
    assert_eq!(workspace_children.files, vec![created_file]);

    let created_doc = docs
        .get_document("created-doc-1")
        .expect("created Google Docs document");
    assert_eq!(created_doc.document_id, "created-doc-1");

    let reconciled = fs::read_to_string(&page_path).expect("read reconciled Google Docs create");
    assert!(
        reconciled.contains("loc:\n  id: created-doc-1"),
        "{reconciled}"
    );
    assert!(reconciled.contains("title: Draft Plan"), "{reconciled}");
    assert!(
        reconciled.contains("Created from filesystem."),
        "{reconciled}"
    );

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status after Google Docs create push");
    assert!(status.clean, "{status:#?}");
    assert_eq!(status.summary.dirty, 0, "{status:#?}");
}

#[test]
fn pull_dirty_page_merges_non_overlapping_blocks_and_conflicts_same_block() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![
        paragraph_block("block-1", "Base intro paragraph."),
        paragraph_block("block-2", "Base detail paragraph."),
    ]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount non-overlapping pull fixture");
    run_pull(&mut store, &connector, &fixture.root).expect("initial pull");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read pulled page");
    let local_marker = format!("Local intro edit {}", unique_suffix());
    let remote_marker = format!("Remote detail edit {}", unique_suffix());
    fs::write(
        &page_path,
        original.replace("Base intro paragraph.", &local_marker),
    )
    .expect("write local non-overlapping edit");
    replace_mutable_paragraph(&api, "block-2", &remote_marker);

    let pull = run_pull(&mut store, &connector, &page_path).expect("pull non-overlapping drift");
    assert!(pull.ok, "{pull:#?}");
    assert_eq!(pull.hydrated, 1, "{pull:#?}");
    assert_eq!(pull.skipped_dirty, 0, "{pull:#?}");
    assert!(pull.conflicts.is_empty(), "{pull:#?}");
    let merged = fs::read_to_string(&page_path).expect("read merged page");
    assert!(merged.contains(&local_marker), "{merged}");
    assert!(merged.contains(&remote_marker), "{merged}");
    assert!(
        !has_unresolved_conflict_markers(&merged),
        "non-overlapping block edits should merge without conflict markers:\n{merged}"
    );
    let merged_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status after non-overlapping merge");
    assert_eq!(merged_status.summary.conflicted, 0, "{merged_status:#?}");
    assert_eq!(merged_status.summary.dirty, 1, "{merged_status:#?}");
    assert_eq!(merged_status.summary.review_needed, 0, "{merged_status:#?}");

    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![
        paragraph_block("block-1", "Shared base paragraph."),
        paragraph_block("block-2", "Unchanged detail paragraph."),
    ]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount same-block pull fixture");
    run_pull(&mut store, &connector, &fixture.root).expect("initial same-block pull");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read same-block page");
    let local_marker = format!("Local same-block edit {}", unique_suffix());
    let remote_marker = format!("Remote same-block edit {}", unique_suffix());
    fs::write(
        &page_path,
        original.replace("Shared base paragraph.", &local_marker),
    )
    .expect("write local same-block edit");
    replace_mutable_paragraph(&api, "block-1", &remote_marker);

    let pull = run_pull(&mut store, &connector, &page_path).expect("pull same-block drift");
    assert!(!pull.ok, "{pull:#?}");
    assert_eq!(pull.hydrated, 0, "{pull:#?}");
    assert_eq!(pull.skipped_dirty, 1, "{pull:#?}");
    assert_eq!(pull.conflicts.len(), 1, "{pull:#?}");
    let conflicted = fs::read_to_string(&page_path).expect("read conflicted page");
    assert!(conflicted.contains(&local_marker), "{conflicted}");
    assert!(conflicted.contains(&remote_marker), "{conflicted}");
    assert!(conflicted.contains(CONFLICT_LOCAL_MARKER), "{conflicted}");
    assert!(
        conflicted.contains(CONFLICT_SEPARATOR_MARKER),
        "{conflicted}"
    );
    assert!(conflicted.contains(CONFLICT_REMOTE_MARKER), "{conflicted}");
    assert!(has_unresolved_conflict_markers(&conflicted), "{conflicted}");
    let conflicted_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path),
            ..StatusOptions::default()
        },
    )
    .expect("status after same-block conflict");
    assert_eq!(
        conflicted_status.summary.conflicted, 1,
        "{conflicted_status:#?}"
    );
}

#[test]
fn conflicted_pull_restore_requires_force_and_restores_remote_shadow() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![
        paragraph_block("block-1", "Shared base paragraph."),
        paragraph_block("block-2", "Unchanged detail paragraph."),
    ]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount restore conflict fixture");
    run_pull(&mut store, &connector, &fixture.root).expect("initial restore conflict pull");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read restore conflict page");
    fs::write(
        &page_path,
        original.replace("Shared base paragraph.", "Local conflicting restore edit."),
    )
    .expect("write local restore conflict edit");
    replace_mutable_paragraph(&api, "block-1", "Remote conflicting restore edit.");

    let pull = run_pull(&mut store, &connector, &page_path).expect("pull restore conflict drift");
    assert!(!pull.ok, "{pull:#?}");
    assert_eq!(pull.conflicts.len(), 1, "{pull:#?}");
    let conflicted = fs::read_to_string(&page_path).expect("read restore conflict markers");
    assert!(has_unresolved_conflict_markers(&conflicted), "{conflicted}");

    let blocked_restore =
        run_restore(&mut store, &page_path, RestoreOptions::default()).expect_err("restore block");
    assert_eq!(
        blocked_restore.code(),
        "restore_conflicted_requires_force",
        "{blocked_restore:?}"
    );
    assert!(
        has_unresolved_conflict_markers(
            &fs::read_to_string(&page_path).expect("read markers after blocked restore")
        ),
        "unforced restore must leave conflict markers intact"
    );

    let restore = run_restore(
        &mut store,
        &page_path,
        RestoreOptions {
            force: true,
            state_root: None,
        },
    )
    .expect("force restore conflict");
    assert!(restore.ok, "{restore:#?}");
    assert_eq!(restore.action, "restored", "{restore:#?}");

    let restored = fs::read_to_string(&page_path).expect("read restored conflict file");
    assert!(
        restored.contains("Remote conflicting restore edit."),
        "{restored}"
    );
    assert!(
        !restored.contains("Local conflicting restore edit."),
        "{restored}"
    );
    assert!(!restored.contains("Shared base paragraph."), "{restored}");
    assert!(
        !has_unresolved_conflict_markers(&restored),
        "forced restore should remove conflict markers:\n{restored}"
    );

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path),
            ..StatusOptions::default()
        },
    )
    .expect("status after force restore conflict");
    assert_eq!(status.summary.conflicted, 0, "{status:#?}");
    assert_eq!(status.summary.dirty, 0, "{status:#?}");
}

#[test]
fn unresolved_pull_conflict_markers_block_push_before_journaled_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![
        paragraph_block("block-1", "Shared base paragraph."),
        paragraph_block("block-2", "Unchanged detail paragraph."),
    ]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount unresolved conflict fixture");
    run_pull(&mut store, &connector, &fixture.root).expect("initial unresolved conflict pull");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read unresolved conflict page");
    let local_marker = format!("Local unresolved conflict edit {}", unique_suffix());
    let remote_marker = format!("Remote unresolved conflict edit {}", unique_suffix());
    fs::write(
        &page_path,
        original.replace("Shared base paragraph.", &local_marker),
    )
    .expect("write local unresolved conflict edit");
    replace_mutable_paragraph(&api, "block-1", &remote_marker);

    let pull = run_pull(&mut store, &connector, &page_path).expect("pull unresolved conflict");
    assert!(!pull.ok, "{pull:#?}");
    assert_eq!(pull.conflicts.len(), 1, "{pull:#?}");
    let conflicted = fs::read_to_string(&page_path).expect("read unresolved conflict markers");
    assert!(conflicted.contains(&local_marker), "{conflicted}");
    assert!(conflicted.contains(&remote_marker), "{conflicted}");
    assert!(has_unresolved_conflict_markers(&conflicted), "{conflicted}");

    let diff = run_diff(&store, &page_path).expect("diff unresolved conflict markers");
    assert!(!diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(diff.validation[0].code, "unresolved_conflict_markers");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push unresolved conflict markers");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(store.list_journal().expect("journal").is_empty());

    let calls = api.calls.lock().expect("calls");
    assert!(
        calls.is_empty(),
        "unresolved conflict markers must block before connector apply: {calls:#?}"
    );
    drop(calls);

    let remote = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new("page-1"),
        })
        .expect("fetch remote after blocked unresolved conflict push");
    let remote_body = connector
        .render_native_entity_for_path(&remote, &page_path)
        .expect("render remote after blocked unresolved conflict push")
        .document
        .body;
    assert!(remote_body.contains(&remote_marker), "{remote_body}");
    assert!(
        !remote_body.contains(&local_marker),
        "blocked unresolved conflict push must not write local marker remotely:\n{remote_body}"
    );
}

#[test]
fn macos_file_provider_visible_conflict_updates_visible_replica_and_cache() {
    visible_projection_conflict_updates_visible_replica_and_cache(
        ProjectionMode::MacosFileProvider,
        "macOS File Provider",
    );
}

#[test]
fn windows_cloud_files_visible_conflict_updates_visible_replica_and_cache() {
    visible_projection_conflict_updates_visible_replica_and_cache(
        ProjectionMode::WindowsCloudFiles,
        "Windows Cloud Files",
    );
}

fn visible_projection_conflict_updates_visible_replica_and_cache(
    projection: ProjectionMode,
    projection_name: &str,
) {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let base = format!("{projection_name} local base paragraph.");
    let api = Arc::new(MutableNotionApi::with_blocks(vec![
        paragraph_block("block-1", &base),
        paragraph_block(
            "block-2",
            &format!("Stable {projection_name} detail paragraph."),
        ),
    ]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount visible projection fixture");
    run_pull_with_state_root(
        &mut store,
        &connector,
        &fixture.root,
        Some(&fixture.state_root),
    )
    .expect("initial pull into daemon content cache");

    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("get File Provider entity")
        .expect("File Provider entity");
    let cache_path = fixture.content_root().join(&entity.path);
    let visible_path = fixture.root.join(&entity.path);
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::copy(&cache_path, &visible_path).expect("seed visible File Provider replica");

    let original = fs::read_to_string(&visible_path).expect("read visible File Provider replica");
    let local_marker = format!(
        "Local visible {projection_name} conflict {}",
        unique_suffix()
    );
    let remote_marker = format!("Remote {projection_name} conflict {}", unique_suffix());
    write_visible_projection_edit_newer_than_cache(
        &visible_path,
        &cache_path,
        &original.replace(&base, &local_marker),
    );
    replace_mutable_paragraph(&api, "block-1", &remote_marker);

    let pull = run_pull_with_state_root(
        &mut store,
        &connector,
        &visible_path,
        Some(&fixture.state_root),
    )
    .expect("pull conflicted visible File Provider replica");
    assert!(!pull.ok, "{pull:#?}");
    assert_eq!(pull.hydrated, 0, "{pull:#?}");
    assert_eq!(pull.skipped_dirty, 1, "{pull:#?}");
    assert_eq!(pull.conflicts.len(), 1, "{pull:#?}");

    let visible = fs::read_to_string(&visible_path).expect("read visible conflicted replica");
    assert!(visible.contains(&local_marker), "{visible}");
    assert!(visible.contains(&remote_marker), "{visible}");
    assert!(visible.contains(CONFLICT_LOCAL_MARKER), "{visible}");
    assert!(visible.contains(CONFLICT_SEPARATOR_MARKER), "{visible}");
    assert!(visible.contains(CONFLICT_REMOTE_MARKER), "{visible}");
    assert!(has_unresolved_conflict_markers(&visible), "{visible}");
    let cached = fs::read_to_string(&cache_path).expect("read daemon conflicted cache");
    assert_eq!(visible, cached);

    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
        .expect("get conflicted File Provider entity")
        .expect("conflicted File Provider entity");
    assert_eq!(entity.hydration, HydrationState::Conflicted);
    let status = run_status(
        &store,
        StatusOptions {
            path: Some(visible_path),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status after File Provider visible conflict");
    assert_eq!(status.summary.conflicted, 1, "{status:#?}");
}

fn write_visible_projection_edit_newer_than_cache(
    visible_path: &Path,
    cache_path: &Path,
    contents: &str,
) {
    let cache_modified = fs::metadata(cache_path)
        .and_then(|metadata| metadata.modified())
        .expect("read daemon content cache mtime");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut attempts = 0usize;

    loop {
        attempts += 1;
        fs::write(visible_path, contents).expect("write missed visible File Provider edit");
        let visible_modified = fs::metadata(visible_path)
            .and_then(|metadata| metadata.modified())
            .expect("read visible File Provider replica mtime");
        if visible_modified > cache_modified {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "visible File Provider replica mtime did not advance past daemon cache mtime after {attempts} writes; visible={visible_modified:?}, cache={cache_modified:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn large_archive_plan_requires_confirm_before_journaled_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(
        (0..12)
            .map(|index| paragraph_block(&format!("block-{index}"), &format!("Paragraph {index}.")))
            .collect(),
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount large archive fixture");
    run_pull(&mut store, &connector, &fixture.root).expect("pull large archive page");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read large archive page");
    let frontmatter_end = original
        .find("\n---\n")
        .map(|index| index + "\n---\n".len())
        .expect("canonical frontmatter terminator");
    fs::write(&page_path, &original[..frontmatter_end]).expect("remove synced body blocks");

    let diff = run_diff(&store, &page_path).expect("diff large archive plan");
    assert!(diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "confirm_dangerous_plan", "{diff:#?}");
    assert_eq!(diff.guardrail.decision, "confirm_required", "{diff:#?}");
    assert!(
        diff.guardrail
            .reasons
            .iter()
            .any(|reason| reason.contains("12 blocks or pages would be archived or replaced")),
        "{diff:#?}"
    );
    let plan = diff.plan.as_ref().expect("large archive plan");
    assert_eq!(plan.summary.blocks_archived, 12, "{plan:#?}");

    let blocked_push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push unconfirmed large archive");
    assert!(!blocked_push.ok, "{blocked_push:#?}");
    assert_eq!(
        blocked_push.action, "confirm_dangerous_plan",
        "{blocked_push:#?}"
    );
    assert_eq!(
        blocked_push.guardrail.decision, "confirm_required",
        "{blocked_push:#?}"
    );
    assert_eq!(blocked_push.journal_status, None, "{blocked_push:#?}");
    assert!(store.list_journal().expect("journal").is_empty());
    assert!(
        api.calls.lock().expect("calls").is_empty(),
        "unconfirmed large archive must not call connector apply"
    );

    let confirmed_push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: true,
        },
    )
    .expect("push confirmed large archive");
    assert!(confirmed_push.ok, "{confirmed_push:#?}");
    assert_eq!(confirmed_push.action, "reconciled", "{confirmed_push:#?}");
    assert_eq!(
        confirmed_push.journal_status.as_deref(),
        Some("reconciled"),
        "{confirmed_push:#?}"
    );
    assert_eq!(confirmed_push.apply_effect_count, 12, "{confirmed_push:#?}");

    let calls = api.calls.lock().expect("calls");
    let deleted = calls
        .iter()
        .filter(|call| matches!(call, WriteCall::Delete { .. }))
        .count();
    assert_eq!(deleted, 12, "{calls:#?}");
    drop(calls);

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path),
            ..StatusOptions::default()
        },
    )
    .expect("status after confirmed large archive");
    assert!(status.clean, "{status:#?}");
    assert_eq!(status.summary.dirty, 0, "{status:#?}");
}

#[test]
fn auto_save_safe_update_reconciles_and_destructive_update_blocks_before_journaled_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(
        (0..12)
            .map(|index| {
                paragraph_block(
                    &format!("autosave-block-{index}"),
                    &format!("Auto-save paragraph {index}."),
                )
            })
            .collect(),
    ));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount auto-save fixture");
    run_pull(&mut store, &connector, &fixture.root).expect("pull auto-save page");

    let page_path = fixture.page_file();
    let relative_path = page_path
        .strip_prefix(&fixture.root)
        .expect("page is under mount root")
        .to_path_buf();
    store
        .save_auto_save_enrollment(
            AutoSaveEnrollmentRecord::new(
                fixture.mount_id.clone(),
                relative_path.clone(),
                AutoSaveOrigin::UserEnabled,
                "unix_ms:1",
            )
            .active("unix_ms:2"),
        )
        .expect("save auto-save enrollment");

    let original = fs::read_to_string(&page_path).expect("read auto-save page");
    assert!(original.contains("Auto-save paragraph 0."), "{original}");
    let safe_marker = format!("Auto-save safe update {}", unique_suffix());
    fs::write(
        &page_path,
        original.replace("Auto-save paragraph 0.", &safe_marker),
    )
    .expect("write auto-save safe edit");

    let safe_report = execute_auto_save_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: page_path.clone(),
            assume_yes: false,
            confirm_dangerous: false,
        },
        &connector,
        None,
    )
    .expect("execute auto-save safe push");
    assert_eq!(safe_report.action, PushJobAction::Reconciled);
    assert!(safe_report.error.is_none(), "{safe_report:#?}");
    assert!(safe_report.push_id.is_some(), "{safe_report:#?}");
    let active_enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, &relative_path)
        .expect("load active auto-save enrollment")
        .expect("active auto-save enrollment");
    assert_eq!(active_enrollment.state, AutoSaveState::Active);
    assert_eq!(active_enrollment.remote_id, Some(RemoteId::new("page-1")));
    assert!(active_enrollment.last_push_id.is_some());
    let calls_after_safe = api.calls.lock().expect("calls").len();
    assert!(calls_after_safe > 0, "safe auto-save should write remotely");
    assert_eq!(store.list_journal().expect("journal").len(), 1);

    let safe_reconciled = fs::read_to_string(&page_path).expect("read safe reconciled page");
    assert!(safe_reconciled.contains(&safe_marker), "{safe_reconciled}");
    let frontmatter_end = safe_reconciled
        .find("\n---\n")
        .map(|index| index + "\n---\n".len())
        .expect("canonical frontmatter terminator");
    fs::write(&page_path, &safe_reconciled[..frontmatter_end])
        .expect("remove synced body for destructive auto-save edit");

    let blocked_report = execute_auto_save_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: page_path.clone(),
            assume_yes: false,
            confirm_dangerous: false,
        },
        &connector,
        None,
    )
    .expect("execute auto-save destructive push");
    assert_eq!(blocked_report.action, PushJobAction::NotReady);
    let blocked_error = blocked_report.error.as_ref().expect("blocked error");
    assert_eq!(blocked_error.code, "auto_save_blocked");
    assert!(
        blocked_error
            .message
            .contains("destructive push plan needs explicit review"),
        "{blocked_report:#?}"
    );
    assert_eq!(blocked_report.push_id, None, "{blocked_report:#?}");
    assert_eq!(blocked_report.journal_status, None, "{blocked_report:#?}");
    let blocked_enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, &relative_path)
        .expect("load blocked auto-save enrollment")
        .expect("blocked auto-save enrollment");
    assert_eq!(blocked_enrollment.state, AutoSaveState::Blocked);
    assert!(
        blocked_enrollment
            .last_reason
            .as_deref()
            .unwrap_or_default()
            .contains("destructive push plan needs explicit review"),
        "{blocked_enrollment:#?}"
    );
    assert_eq!(
        store
            .list_journal()
            .expect("journal after blocked auto-save")
            .len(),
        1,
        "blocked auto-save must not create a second journal entry"
    );
    assert_eq!(
        api.calls.lock().expect("calls").len(),
        calls_after_safe,
        "blocked auto-save must not call connector apply"
    );
}

#[test]
fn cli_live_mode_toggles_file_auto_save_enrollment() {
    let fixture = E2eFixture::new();
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).expect("open state");
    let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
        "cli-live-mode-block",
        "CLI Live Mode paragraph.",
    )]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api);

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live-mode CLI fixture");
    run_pull(&mut store, &connector, &fixture.root).expect("pull live-mode CLI page");

    let page_path = fixture.page_file();
    let page_arg = page_path.display().to_string();
    let relative_path = page_path
        .strip_prefix(&fixture.root)
        .expect("page is under mount root")
        .to_path_buf();
    store
        .save_auto_save_enrollment(
            AutoSaveEnrollmentRecord::new(
                fixture.mount_id.clone(),
                relative_path.clone(),
                AutoSaveOrigin::UserEnabled,
                "unix_ms:1",
            )
            .paused_remote_changed("remote changed before enabling", "unix_ms:2"),
        )
        .expect("seed paused enrollment");
    drop(store);

    let loc = env!("CARGO_BIN_EXE_loc");
    let enabled = loc_json_ok(loc_command(loc, &fixture.state_root).args([
        "live-mode",
        "on",
        &page_arg,
        "--json",
    ]));
    assert_eq!(enabled.value["ok"], true, "{enabled:#?}");
    assert_eq!(enabled.value["command"], "live_mode", "{enabled:#?}");
    assert_eq!(enabled.value["action"], "enabled", "{enabled:#?}");
    assert_eq!(
        enabled.value["mount_id"],
        fixture.mount_id.as_str(),
        "{enabled:#?}"
    );
    assert_eq!(
        enabled.value["relative_path"],
        relative_path.display().to_string(),
        "{enabled:#?}"
    );
    assert_eq!(enabled.value["remote_id"], "page-1", "{enabled:#?}");
    assert_eq!(enabled.value["enabled"], true, "{enabled:#?}");
    assert_eq!(enabled.value["state"], "active", "{enabled:#?}");
    assert_eq!(enabled.value["origin"], "user_enabled", "{enabled:#?}");
    assert_eq!(enabled.value["reason"], Value::Null, "{enabled:#?}");

    let store = SqliteStateStore::open(fixture.state_root.clone()).expect("reopen state");
    let enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, &relative_path)
        .expect("load enrollment")
        .expect("enrollment");
    assert!(enrollment.enabled, "{enrollment:#?}");
    assert_eq!(enrollment.state, AutoSaveState::Active);
    assert_eq!(enrollment.origin, AutoSaveOrigin::UserEnabled);
    assert_eq!(enrollment.last_reason, None);
    assert_eq!(enrollment.remote_id, Some(RemoteId::new("page-1")));
    drop(store);

    let status = loc_json_ok(loc_command(loc, &fixture.state_root).args([
        "live-mode",
        "status",
        &page_arg,
        "--json",
    ]));
    assert_eq!(status.value["ok"], true, "{status:#?}");
    assert_eq!(status.value["action"], "status", "{status:#?}");
    assert_eq!(status.value["enabled"], true, "{status:#?}");
    assert_eq!(status.value["state"], "active", "{status:#?}");

    let disabled = loc_json_ok(loc_command(loc, &fixture.state_root).args([
        "live-mode",
        "off",
        &page_arg,
        "--json",
    ]));
    assert_eq!(disabled.value["ok"], true, "{disabled:#?}");
    assert_eq!(disabled.value["action"], "disabled", "{disabled:#?}");
    assert_eq!(disabled.value["enabled"], false, "{disabled:#?}");
    assert_eq!(disabled.value["state"], "active", "{disabled:#?}");
    assert_eq!(disabled.value["origin"], "user_enabled", "{disabled:#?}");

    let store = SqliteStateStore::open(fixture.state_root.clone()).expect("reopen state");
    let enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, &relative_path)
        .expect("load disabled enrollment")
        .expect("disabled enrollment");
    assert!(!enrollment.enabled, "{enrollment:#?}");
    assert_eq!(enrollment.state, AutoSaveState::Active);
    assert_eq!(enrollment.origin, AutoSaveOrigin::UserEnabled);
    assert_eq!(enrollment.last_reason, None);
}

#[test]
fn cli_live_mode_page_directory_targets_known_page_document_without_materialized_directory() {
    let fixture = E2eFixture::new();
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).expect("open state");
    fs::create_dir_all(&fixture.root).expect("mount root");
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live-mode page directory fixture");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save page entity");
    drop(store);

    let loc = env!("CARGO_BIN_EXE_loc");
    let page_dir = fixture.root.join("Roadmap");
    let page_dir_arg = page_dir.display().to_string();
    let expected_page_path = fixture.root.join("Roadmap/page.md");
    let expected_relative_path = PathBuf::from("Roadmap/page.md");
    let enabled = loc_json_ok(loc_command(loc, &fixture.state_root).args([
        "live-mode",
        "on",
        &page_dir_arg,
        "--json",
    ]));

    assert_eq!(enabled.value["ok"], true, "{enabled:#?}");
    assert_eq!(
        enabled.value["path"],
        expected_page_path.display().to_string()
    );
    assert_eq!(
        enabled.value["relative_path"],
        expected_relative_path.display().to_string(),
        "{enabled:#?}"
    );
    assert_eq!(enabled.value["remote_id"], "page-1", "{enabled:#?}");

    let store = SqliteStateStore::open(fixture.state_root.clone()).expect("reopen state");
    assert!(
        store
            .get_auto_save_enrollment(&fixture.mount_id, Path::new("Roadmap"))
            .expect("load wrong path enrollment")
            .is_none(),
        "page directory target must not enroll the container path"
    );
    let enrollment = store
        .get_auto_save_enrollment(&fixture.mount_id, &expected_relative_path)
        .expect("load page document enrollment")
        .expect("page document enrollment");
    assert!(enrollment.enabled, "{enrollment:#?}");
    assert_eq!(enrollment.remote_id, Some(RemoteId::new("page-1")));
}

#[test]
fn cli_live_mode_rejects_mount_directory_without_page_file() {
    let fixture = E2eFixture::new();
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).expect("open state");
    fs::create_dir_all(&fixture.root).expect("mount root");
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live-mode directory fixture");
    drop(store);

    let loc = env!("CARGO_BIN_EXE_loc");
    let root_arg = fixture.root.display().to_string();
    let rejected = loc_json_with_exit(
        loc_command(loc, &fixture.state_root).args(["live-mode", "on", &root_arg, "--json"]),
        2,
    );
    assert_eq!(rejected.value["ok"], false, "{rejected:#?}");
    assert_eq!(rejected.value["command"], "live_mode", "{rejected:#?}");
    assert_eq!(
        rejected.value["code"], "unsupported_target",
        "{rejected:#?}"
    );
    assert!(
        rejected.value["message"]
            .as_str()
            .expect("message")
            .contains("not a file or known page directory"),
        "{rejected:#?}"
    );

    let store = SqliteStateStore::open(fixture.state_root.clone()).expect("reopen state");
    assert!(
        store
            .list_auto_save_enrollments(&fixture.mount_id)
            .expect("list enrollments")
            .is_empty(),
        "directory rejection must not create an enrollment"
    );
}

#[test]
fn mount_pull_directive_move_pushes_copy_archive_and_status_clean() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![
        paragraph_block("block-1", "First paragraph."),
        synced_block("synced-1", "source-block-1"),
    ]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount");

    run_pull(&mut store, &connector, &fixture.root).expect("pull");
    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read pulled page");
    let directive_line = original
        .lines()
        .find(|line| line.contains("id=synced-1"))
        .expect("synced block directive line");
    let original_order = format!("First paragraph.\n\n{directive_line}\n");
    assert!(original.contains(&original_order), "{original}");
    fs::write(
        &page_path,
        original.replace(
            &original_order,
            &format!("{directive_line}\n\nFirst paragraph.\n"),
        ),
    )
    .expect("write directive move");

    let diff = run_diff(&store, &page_path).expect("diff directive move");
    let plan = diff.plan.as_ref().expect("plan");
    assert_eq!(diff.action, "confirm_plan");
    assert_eq!(plan.summary.blocks_created, 0, "{plan:#?}");
    assert_eq!(plan.summary.blocks_moved, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push directive move");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.apply_effect_count, 2);

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status");
    assert!(status.clean, "{status:#?}");
    assert_eq!(status.summary.dirty, 0);

    let calls = api.calls.lock().expect("calls");
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, WriteCall::Append { .. })),
        "{calls:#?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, WriteCall::Delete { block_id } if block_id == "synced-1")),
        "{calls:#?}"
    );
}

#[test]
fn notion_table_header_mode_change_blocks_before_journaled_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::new());
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount Notion table header-mode guardrail fixture");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save table page entity");

    let original_body = "|  |  |\n| --- | --- |\n| Old task | Todo |";
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        original_body,
        8,
        [RemoteId::new("table-1")],
    )
    .expect("table shadow");
    shadow.blocks[0].kind = MarkdownBlockKind::TableWithRows {
        row_ids: vec![RemoteId::new("row-1")],
        has_column_header: false,
        has_row_header: false,
    };
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save table shadow");

    let edited_body = "| Name | Status |\n| --- | --- |\n| Old task | Todo |";
    let page_path = fixture.root.join("Roadmap/page.md");
    fs::create_dir_all(page_path.parent().expect("page parent")).expect("create page parent");
    fs::write(
        &page_path,
        render_canonical_markdown(&CanonicalDocument::new(
            "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
            edited_body,
        )),
    )
    .expect("write table header-mode edit");

    let diff = run_diff(&store, &page_path).expect("diff table header-mode edit");
    assert!(!diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(
        diff.validation[0].code,
        "notion_table_header_mode_change_unsupported"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push table header-mode edit");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert!(push.plan.is_none(), "{push:#?}");
    assert_eq!(
        push.validation[0].code,
        "notion_table_header_mode_change_unsupported"
    );
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(
        store.list_journal().expect("journal").is_empty(),
        "table header-mode validation must block before journal creation"
    );
    let calls = api.calls.lock().expect("calls");
    assert!(
        calls.is_empty(),
        "table header-mode validation must block before connector apply: {calls:#?}"
    );
}

#[test]
fn notion_link_preview_edit_move_delete_block_before_journaled_apply() {
    let cases = [
        (
            "edit",
            "[Preview](https://example.com/preview)",
            "[Changed](https://example.com/preview)",
            0,
            "notion_link_preview_edit_unsupported",
        ),
        (
            "move",
            "Intro.\n\n[Preview](https://example.com/preview)",
            "[Preview](https://example.com/preview)\n\nIntro.",
            1,
            "notion_link_preview_move_unsupported",
        ),
        (
            "delete",
            "Intro.\n\n[Preview](https://example.com/preview)",
            "Intro.",
            1,
            "notion_link_preview_delete_unsupported",
        ),
    ];

    for (name, original_body, edited_body, link_preview_index, expected_code) in cases {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        let api = Arc::new(MutableNotionApi::new());
        let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

        run_mount(
            &mut store,
            MountOptions {
                mount_id: fixture.mount_id.clone(),
                connector: "notion".to_string(),
                root: fixture.root.clone(),
                remote_root_id: Some(RemoteId::new("page-1")),
                connection_id: Some(ConnectionId::new("work")),
                read_only: false,
                projection: ProjectionMode::PlainFiles,
                settings_json: "{}".to_string(),
            },
        )
        .unwrap_or_else(|error| panic!("mount link_preview {name} guardrail fixture: {error:?}"));
        store
            .save_entity(
                EntityRecord::new(
                    fixture.mount_id.clone(),
                    RemoteId::new("page-1"),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .unwrap_or_else(|error| panic!("save link_preview {name} page entity: {error}"));

        let native_ids = if link_preview_index == 0 {
            vec![RemoteId::new("link-preview-1")]
        } else {
            vec![
                RemoteId::new("paragraph-1"),
                RemoteId::new("link-preview-1"),
            ]
        };
        let mut shadow =
            ShadowDocument::from_synced_body(RemoteId::new("page-1"), original_body, 8, native_ids)
                .unwrap_or_else(|error| panic!("link_preview {name} shadow: {error}"));
        shadow.blocks[link_preview_index].native_kind = Some("link_preview".to_string());
        store
            .save_shadow(&fixture.mount_id, shadow)
            .unwrap_or_else(|error| panic!("save link_preview {name} shadow: {error}"));

        let page_path = fixture.root.join("Roadmap/page.md");
        fs::create_dir_all(page_path.parent().expect("page parent"))
            .unwrap_or_else(|error| panic!("create link_preview {name} page parent: {error}"));
        fs::write(
            &page_path,
            render_canonical_markdown(&CanonicalDocument::new(
                "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
                edited_body,
            )),
        )
        .unwrap_or_else(|error| panic!("write link_preview {name} edit: {error}"));

        let diff = run_diff(&store, &page_path)
            .unwrap_or_else(|error| panic!("diff link_preview {name}: {error:?}"));
        assert!(!diff.ok, "{name}: {diff:#?}");
        assert_eq!(diff.action, "fix_validation", "{name}: {diff:#?}");
        assert!(diff.plan.is_none(), "{name}: {diff:#?}");
        assert_eq!(diff.validation[0].code, expected_code, "{name}: {diff:#?}");

        let push = run_push_with_daemon(
            &mut store,
            &connector,
            &page_path,
            PushOptions {
                assume_yes: true,
                confirm_dangerous: false,
            },
        )
        .unwrap_or_else(|error| panic!("push link_preview {name}: {error}"));
        assert!(!push.ok, "{name}: {push:#?}");
        assert_eq!(push.action, "fix_validation", "{name}: {push:#?}");
        assert!(push.plan.is_none(), "{name}: {push:#?}");
        assert_eq!(push.validation[0].code, expected_code, "{name}: {push:#?}");
        assert_eq!(push.push_id, None, "{name}: {push:#?}");
        assert_eq!(push.journal_status, None, "{name}: {push:#?}");
        assert!(
            store.list_journal().expect("journal").is_empty(),
            "{name}: link_preview validation must block before journal creation"
        );
        let calls = api.calls.lock().expect("calls");
        assert!(
            calls.is_empty(),
            "{name}: link_preview validation must block before connector apply: {calls:#?}"
        );
    }
}

#[test]
fn notion_link_to_page_label_edit_blocks_before_journaled_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::new());
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let target_id = "22222222-2222-2222-2222-222222222222";

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount link-to-page edit guardrail fixture");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save link-to-page parent entity");

    let original_body = "[Linked page](https://www.notion.so/22222222222222222222222222222222)";
    let mut shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        original_body,
        8,
        [RemoteId::new("link-to-page-block-1")],
    )
    .expect("link-to-page shadow");
    shadow.blocks[0].native_kind = Some("link_to_page".to_string());
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save link-to-page shadow");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new(target_id),
                EntityKind::Page,
                "Linked page",
                "Linked page/page.md",
            )
            .with_hydration(HydrationState::Stub),
        )
        .expect("save link-to-page target entity");

    let edited_body =
        "[Edited linked page](https://www.notion.so/22222222222222222222222222222222)";
    let page_path = fixture.root.join("Roadmap/page.md");
    fs::create_dir_all(page_path.parent().expect("page parent")).expect("create page parent");
    fs::write(
        &page_path,
        render_canonical_markdown(&CanonicalDocument::new(
            "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
            edited_body,
        )),
    )
    .expect("write link-to-page label edit");

    let diff = run_diff(&store, &page_path).expect("diff link-to-page label edit");
    assert!(!diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(
        diff.validation[0].code,
        "notion_link_to_page_edit_unsupported"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push link-to-page label edit");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert!(push.plan.is_none(), "{push:#?}");
    assert_eq!(
        push.validation[0].code,
        "notion_link_to_page_edit_unsupported"
    );
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(
        store.list_journal().expect("journal").is_empty(),
        "link-to-page validation must block before journal creation"
    );
    let calls = api.calls.lock().expect("calls");
    assert!(
        calls.is_empty(),
        "link-to-page validation must block before connector apply: {calls:#?}"
    );
}

#[test]
fn notion_child_page_link_label_edit_blocks_before_journaled_apply() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::new());
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());
    let child_id = "11111111-1111-1111-1111-111111111111";

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount child-page link edit guardrail fixture");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save parent page entity");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new(child_id),
                EntityKind::Page,
                "Child Page",
                "Roadmap/child-page/page.md",
            )
            .with_hydration(HydrationState::Stub),
        )
        .expect("save child page entity");

    let original_body = "[Child Page](https://www.notion.so/11111111111111111111111111111111)";
    let shadow = ShadowDocument::from_synced_body(
        RemoteId::new("page-1"),
        original_body,
        8,
        [RemoteId::new(child_id)],
    )
    .expect("child-page link shadow");
    store
        .save_shadow(&fixture.mount_id, shadow)
        .expect("save child-page link shadow");

    let edited_body = "[Edited Child Page](https://www.notion.so/11111111111111111111111111111111)";
    let page_path = fixture.root.join("Roadmap/page.md");
    fs::create_dir_all(page_path.parent().expect("page parent")).expect("create page parent");
    fs::write(
        &page_path,
        render_canonical_markdown(&CanonicalDocument::new(
            "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
            edited_body,
        )),
    )
    .expect("write child-page link label edit");

    let diff = run_diff(&store, &page_path).expect("diff child-page link label edit");
    assert!(!diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(
        diff.validation[0].code,
        "notion_child_page_link_edit_unsupported"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push child-page link label edit");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert!(push.plan.is_none(), "{push:#?}");
    assert_eq!(
        push.validation[0].code,
        "notion_child_page_link_edit_unsupported"
    );
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(
        store.list_journal().expect("journal").is_empty(),
        "child-page link validation must block before journal creation"
    );
    let calls = api.calls.lock().expect("calls");
    assert!(
        calls.is_empty(),
        "child-page link validation must block before connector apply: {calls:#?}"
    );
}

#[test]
fn google_docs_rendered_inline_object_and_table_guardrails_block_before_journaled_apply() {
    let image = "![A circle with logo written in the center](https://example.test/circle.png)";
    let table = "| Pet | Age |\n| --- | --- |\n| Luna | 4 |";
    let cases = vec![
        (
            "inline_object_edit",
            image.to_string(),
            "![Edited image](https://example.test/circle.png)".to_string(),
            0,
            "google_docs_inline_object",
            "google_docs_inline_object_edit_unsupported",
        ),
        (
            "inline_object_move",
            format!("Intro.\n\n{image}"),
            format!("{image}\n\nIntro."),
            1,
            "google_docs_inline_object",
            "google_docs_inline_object_move_unsupported",
        ),
        (
            "inline_object_delete",
            format!("{image}\n\n{image}"),
            image.to_string(),
            1,
            "google_docs_inline_object",
            "google_docs_inline_object_delete_unsupported",
        ),
        (
            "table_edit",
            table.to_string(),
            "| Pet | Age |\n| --- | --- |\n| Luna | 5 |".to_string(),
            0,
            "google_docs_table",
            "google_docs_table_edit_unsupported",
        ),
        (
            "table_move",
            format!("Intro.\n\n{table}"),
            format!("{table}\n\nIntro."),
            1,
            "google_docs_table",
            "google_docs_table_move_unsupported",
        ),
    ];

    for (name, original_body, edited_body, rendered_block_index, native_kind, expected_code) in
        cases
    {
        let fixture = E2eFixture::new();
        let mount_id = MountId::new("google-docs-main");
        let mut store = InMemoryStateStore::new();
        let connector = BlockingGuardrailConnector::default();

        run_mount(
            &mut store,
            MountOptions {
                mount_id: mount_id.clone(),
                connector: "google-docs".to_string(),
                root: fixture.root.clone(),
                remote_root_id: Some(RemoteId::new("workspace-folder-1")),
                connection_id: Some(ConnectionId::new("google-docs-work")),
                read_only: false,
                projection: ProjectionMode::PlainFiles,
                settings_json: "{}".to_string(),
            },
        )
        .unwrap_or_else(|error| panic!("mount Google Docs {name} guardrail fixture: {error:?}"));
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("doc-1"),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .unwrap_or_else(|error| panic!("save Google Docs {name} entity: {error}"));

        let native_ids = if rendered_block_index == 0 {
            vec![RemoteId::new("doc-1:rendered")]
        } else {
            vec![
                RemoteId::new("doc-1:intro"),
                RemoteId::new("doc-1:rendered"),
            ]
        };
        let mut shadow =
            ShadowDocument::from_synced_body(RemoteId::new("doc-1"), &original_body, 8, native_ids)
                .unwrap_or_else(|error| panic!("Google Docs {name} shadow: {error}"));
        shadow.blocks[rendered_block_index].native_kind = Some(native_kind.to_string());
        store
            .save_shadow(&mount_id, shadow)
            .unwrap_or_else(|error| panic!("save Google Docs {name} shadow: {error}"));

        let page_path = fixture.root.join("Roadmap/page.md");
        fs::create_dir_all(page_path.parent().expect("page parent"))
            .unwrap_or_else(|error| panic!("create Google Docs {name} page parent: {error}"));
        fs::write(
            &page_path,
            render_canonical_markdown(&CanonicalDocument::new(
                "loc:\n  id: doc-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n",
                &edited_body,
            )),
        )
        .unwrap_or_else(|error| panic!("write Google Docs {name} edit: {error}"));

        let diff = run_diff(&store, &page_path)
            .unwrap_or_else(|error| panic!("diff Google Docs {name}: {error:?}"));
        assert!(!diff.ok, "{name}: {diff:#?}");
        assert_eq!(diff.action, "fix_validation", "{name}: {diff:#?}");
        assert!(diff.plan.is_none(), "{name}: {diff:#?}");
        assert_eq!(diff.validation[0].code, expected_code, "{name}: {diff:#?}");

        let push = run_push_with_daemon(
            &mut store,
            &connector,
            &page_path,
            PushOptions {
                assume_yes: true,
                confirm_dangerous: false,
            },
        )
        .unwrap_or_else(|error| panic!("push Google Docs {name}: {error}"));
        assert!(!push.ok, "{name}: {push:#?}");
        assert_eq!(push.action, "fix_validation", "{name}: {push:#?}");
        assert!(push.plan.is_none(), "{name}: {push:#?}");
        assert_eq!(push.validation[0].code, expected_code, "{name}: {push:#?}");
        assert_eq!(push.push_id, None, "{name}: {push:#?}");
        assert_eq!(push.journal_status, None, "{name}: {push:#?}");
        assert!(
            store.list_journal().expect("journal").is_empty(),
            "{name}: Google Docs validation must block before journal creation"
        );
        assert_eq!(
            connector.concurrency_checks.load(Ordering::Relaxed),
            0,
            "{name}: validation must block before connector concurrency checks"
        );
        assert_eq!(
            connector.apply_calls.load(Ordering::Relaxed),
            0,
            "{name}: validation must block before connector apply"
        );
    }
}

#[test]
fn google_docs_frontmatter_and_unsupported_structure_blocks_before_journaled_apply() {
    let unsupported_directive =
        "::loc{id=doc-1:unsupported type=google_docs_unsupported kind=\"section_break\"}";
    let cases = vec![
        (
            "invalid_entity_type",
            "Original paragraph.".to_string(),
            "Edited paragraph.".to_string(),
            "loc:\n  id: doc-1\n  type: database\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n"
                .to_string(),
            vec![RemoteId::new("doc-1:paragraph")],
            "google_docs_invalid_entity_type",
        ),
        (
            "unsupported_directive_present",
            format!("Original paragraph.\n\n{unsupported_directive}\n"),
            format!("Edited paragraph.\n\n{unsupported_directive}\n"),
            "loc:\n  id: doc-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n"
                .to_string(),
            vec![RemoteId::new("doc-1:paragraph")],
            "google_docs_unsupported_document_structure",
        ),
        (
            "unsupported_directive_removed",
            format!("Original paragraph.\n\n{unsupported_directive}\n"),
            "Edited paragraph.".to_string(),
            "loc:\n  id: doc-1\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n"
                .to_string(),
            vec![RemoteId::new("doc-1:paragraph")],
            "google_docs_unsupported_document_structure",
        ),
    ];

    for (name, original_body, edited_body, frontmatter, native_ids, expected_code) in cases {
        let fixture = E2eFixture::new();
        let mount_id = MountId::new("google-docs-main");
        let mut store = InMemoryStateStore::new();
        let connector = BlockingGuardrailConnector::default();

        run_mount(
            &mut store,
            MountOptions {
                mount_id: mount_id.clone(),
                connector: "google-docs".to_string(),
                root: fixture.root.clone(),
                remote_root_id: Some(RemoteId::new("workspace-folder-1")),
                connection_id: Some(ConnectionId::new("google-docs-work")),
                read_only: false,
                projection: ProjectionMode::PlainFiles,
                settings_json: "{}".to_string(),
            },
        )
        .unwrap_or_else(|error| panic!("mount Google Docs frontmatter {name}: {error:?}"));
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("doc-1"),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .unwrap_or_else(|error| panic!("save Google Docs frontmatter {name} entity: {error}"));

        let shadow =
            ShadowDocument::from_synced_body(RemoteId::new("doc-1"), &original_body, 8, native_ids)
                .unwrap_or_else(|error| panic!("Google Docs frontmatter {name} shadow: {error}"));
        store
            .save_shadow(&mount_id, shadow)
            .unwrap_or_else(|error| panic!("save Google Docs frontmatter {name} shadow: {error}"));

        let page_path = fixture.root.join("Roadmap/page.md");
        fs::create_dir_all(page_path.parent().expect("page parent")).unwrap_or_else(|error| {
            panic!("create Google Docs frontmatter {name} parent: {error}")
        });
        fs::write(
            &page_path,
            render_canonical_markdown(&CanonicalDocument::new(frontmatter, edited_body)),
        )
        .unwrap_or_else(|error| panic!("write Google Docs frontmatter {name} edit: {error}"));

        let diff = run_diff(&store, &page_path)
            .unwrap_or_else(|error| panic!("diff Google Docs frontmatter {name}: {error:?}"));
        assert!(!diff.ok, "{name}: {diff:#?}");
        assert_eq!(diff.action, "fix_validation", "{name}: {diff:#?}");
        assert!(diff.plan.is_none(), "{name}: {diff:#?}");
        assert_eq!(diff.validation[0].code, expected_code, "{name}: {diff:#?}");

        let push = run_push_with_daemon(
            &mut store,
            &connector,
            &page_path,
            PushOptions {
                assume_yes: true,
                confirm_dangerous: false,
            },
        )
        .unwrap_or_else(|error| panic!("push Google Docs frontmatter {name}: {error}"));
        assert!(!push.ok, "{name}: {push:#?}");
        assert_eq!(push.action, "fix_validation", "{name}: {push:#?}");
        assert!(push.plan.is_none(), "{name}: {push:#?}");
        assert_eq!(push.validation[0].code, expected_code, "{name}: {push:#?}");
        assert_eq!(push.push_id, None, "{name}: {push:#?}");
        assert_eq!(push.journal_status, None, "{name}: {push:#?}");
        assert!(
            store.list_journal().expect("journal").is_empty(),
            "{name}: Google Docs frontmatter validation must block before journal creation"
        );
        assert_eq!(
            connector.concurrency_checks.load(Ordering::Relaxed),
            0,
            "{name}: validation must block before connector concurrency checks"
        );
        assert_eq!(
            connector.apply_calls.load(Ordering::Relaxed),
            0,
            "{name}: validation must block before connector apply"
        );
    }
}

#[test]
fn virtual_projection_modes_restore_discards_cached_edit_and_status_returns_clean() {
    for projection in [
        ProjectionMode::MacosFileProvider,
        ProjectionMode::LinuxFuse,
        ProjectionMode::WindowsCloudFiles,
    ] {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
            "block-1",
            "Virtual restore synced body.",
        )]));
        let connector = NotionConnector::with_api(
            NotionConfig::default().with_root_page_id(RemoteId::new("page-1")),
            api,
        );
        mount_virtual_workspace_with_projection(&fixture, &mut store, "page-1", projection.clone());
        let content_root = fixture.content_root();
        hydrate_virtual_root_page(&fixture, &mut store, &connector, &content_root, "page-1");
        let entity = store
            .get_entity(&fixture.mount_id, &RemoteId::new("page-1"))
            .expect("get virtual restore entity")
            .expect("virtual restore entity");
        let visible_page_path = fixture.root.join(&entity.path);
        let cache_page_path = content_root.join(&entity.path);
        let synced = fs::read_to_string(&cache_page_path).expect("read hydrated cache");
        assert!(
            synced.contains("Virtual restore synced body."),
            "{projection:?}: {synced}"
        );

        let local_edit = synced.replace(
            "Virtual restore synced body.",
            "Virtual restore local cache-only edit.",
        );
        fs::write(&cache_page_path, &local_edit).expect("write virtual cache edit");
        let dirty = run_status(
            &store,
            StatusOptions {
                path: Some(visible_page_path.clone()),
                state_root: Some(fixture.state_root.clone()),
                ..StatusOptions::default()
            },
        )
        .expect("status before virtual restore");
        assert_eq!(dirty.summary.dirty, 1, "{projection:?}: {dirty:#?}");

        let restore = run_restore(
            &mut store,
            &visible_page_path,
            RestoreOptions {
                force: false,
                state_root: Some(fixture.state_root.clone()),
            },
        )
        .expect("virtual restore");
        assert!(restore.ok, "{projection:?}: {restore:#?}");
        assert_eq!(restore.action, "restored", "{projection:?}: {restore:#?}");
        assert_eq!(
            restore.mount_id,
            fixture.mount_id.as_str(),
            "{projection:?}: {restore:#?}"
        );

        let restored = fs::read_to_string(&cache_page_path).expect("read restored cache");
        assert!(
            restored.contains("Virtual restore synced body."),
            "{projection:?}: {restored}"
        );
        assert!(
            !restored.contains("Virtual restore local cache-only edit."),
            "{projection:?}: {restored}"
        );
        assert!(
            !visible_page_path.exists(),
            "{projection:?}: restore for a virtual projection should update the daemon content cache, not create a plain mount file"
        );

        let clean = run_status(
            &store,
            StatusOptions {
                path: Some(visible_page_path),
                state_root: Some(fixture.state_root.clone()),
                ..StatusOptions::default()
            },
        )
        .expect("status after virtual restore");
        assert!(clean.clean, "{projection:?}: {clean:#?}");
        assert_eq!(clean.summary.dirty, 0, "{projection:?}: {clean:#?}");
    }
}

#[test]
fn virtual_projection_modes_page_directory_pull_recursively_hydrates_descendant_pages() {
    for projection in [
        ProjectionMode::MacosFileProvider,
        ProjectionMode::LinuxFuse,
        ProjectionMode::WindowsCloudFiles,
    ] {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        mount_virtual_workspace_with_projection(
            &fixture,
            &mut store,
            "project-page",
            projection.clone(),
        );
        let connector = NotionConnector::with_api(
            NotionConfig::default(),
            Arc::new(RecursivePageDirectoryNotionApi::new()),
        );

        let initial_pull = run_pull_with_state_root(
            &mut store,
            &connector,
            &fixture.root,
            Some(&fixture.state_root),
        )
        .unwrap_or_else(|error| panic!("{projection:?}: pull virtual root: {error:?}"));
        assert!(initial_pull.ok, "{projection:?}: {initial_pull:#?}");

        let root_entity = store
            .get_entity(&fixture.mount_id, &RemoteId::new("project-page"))
            .unwrap_or_else(|error| panic!("{projection:?}: get project page: {error}"))
            .unwrap_or_else(|| panic!("{projection:?}: missing project page entity"));
        let root_directory = root_entity
            .path
            .parent()
            .unwrap_or_else(|| panic!("{projection:?}: project page has no directory"))
            .to_path_buf();

        let directory_pull = run_pull_with_state_root(
            &mut store,
            &connector,
            fixture.root.join(&root_directory),
            Some(&fixture.state_root),
        )
        .unwrap_or_else(|error| panic!("{projection:?}: pull project page directory: {error:?}"));

        assert!(directory_pull.ok, "{projection:?}: {directory_pull:#?}");
        assert_eq!(
            directory_pull.enumerated, 2,
            "{projection:?}: {directory_pull:#?}"
        );
        assert_eq!(
            directory_pull.hydrated, 3,
            "{projection:?}: {directory_pull:#?}"
        );
        assert_eq!(
            directory_pull.skipped_dirty, 0,
            "{projection:?}: {directory_pull:#?}"
        );

        assert_hydrated_virtual_page(
            &store,
            &fixture,
            &projection,
            "design-notes-page",
            "Design notes child body.",
        );
        assert_hydrated_virtual_page(
            &store,
            &fixture,
            &projection,
            "appendix-page",
            "Appendix nested child body.",
        );
    }
}

#[test]
fn virtual_projection_modes_surface_pending_create_rename_delete_in_status_and_diff() {
    for projection in [
        ProjectionMode::MacosFileProvider,
        ProjectionMode::LinuxFuse,
        ProjectionMode::WindowsCloudFiles,
    ] {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        run_mount(
            &mut store,
            MountOptions {
                mount_id: fixture.mount_id.clone(),
                connector: "notion".to_string(),
                root: fixture.root.clone(),
                remote_root_id: Some(RemoteId::new("page-1")),
                connection_id: Some(ConnectionId::new("work")),
                read_only: false,
                projection: projection.clone(),
                settings_json: "{}".to_string(),
            },
        )
        .unwrap_or_else(|error| panic!("mount {projection:?} virtual projection: {error:?}"));
        seed_virtual_page(
            &mut store,
            &fixture,
            "page-1",
            "Home",
            "Home/page.md",
            "Home body.",
        );
        seed_virtual_page(
            &mut store,
            &fixture,
            "child-rename",
            "Child Rename",
            "Home/Child Rename/page.md",
            "Child rename body.",
        );
        seed_virtual_page(
            &mut store,
            &fixture,
            "child-delete",
            "Child Delete",
            "Home/Child Delete/page.md",
            "Child delete body.",
        );

        let content_root = fixture.content_root();
        let draft = create_virtual_fs_file(
            &mut store,
            &content_root,
            &fixture.mount_id,
            "children:page-1",
            "Draft.md",
        )
        .unwrap_or_else(|error| panic!("{projection:?}: create draft file: {error:?}"));
        commit_virtual_fs_write(
            &mut store,
            &content_root,
            &fixture.mount_id,
            &draft.identifier,
            render_canonical_markdown(&CanonicalDocument::new(
                "title: Draft\n",
                "Draft body.\n".to_string(),
            ))
            .as_bytes(),
        )
        .unwrap_or_else(|error| panic!("{projection:?}: write draft file: {error:?}"));

        let pending_page = create_virtual_fs_directory(
            &mut store,
            &content_root,
            &fixture.mount_id,
            "children:page-1",
            "Pending Page",
        )
        .unwrap_or_else(|error| panic!("{projection:?}: create pending page directory: {error:?}"));
        assert!(
            pending_page.identifier.starts_with("children:local:"),
            "{projection:?}: {pending_page:#?}"
        );
        let renamed_pending_page = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &fixture.mount_id,
            &pending_page.identifier,
            "children:page-1",
            "Renamed Pending Page",
        )
        .unwrap_or_else(|error| panic!("{projection:?}: rename pending page directory: {error:?}"));
        assert_eq!(
            renamed_pending_page.identifier, pending_page.identifier,
            "{projection:?}: {renamed_pending_page:#?}"
        );
        trash_virtual_fs_item(
            &mut store,
            &content_root,
            &fixture.mount_id,
            &renamed_pending_page.identifier,
        )
        .unwrap_or_else(|error| panic!("{projection:?}: delete pending page directory: {error:?}"));

        let renamed = rename_virtual_fs_item(
            &mut store,
            &content_root,
            &fixture.mount_id,
            "children:child-rename",
            "children:page-1",
            "Renamed Child",
        )
        .unwrap_or_else(|error| panic!("{projection:?}: rename remote child page: {error:?}"));
        assert_eq!(renamed.identifier, "children:child-rename");
        trash_virtual_fs_item(
            &mut store,
            &content_root,
            &fixture.mount_id,
            "children:child-delete",
        )
        .unwrap_or_else(|error| panic!("{projection:?}: delete remote child page: {error:?}"));

        let status = run_status(
            &store,
            StatusOptions {
                path: Some(fixture.root.clone()),
                state_root: Some(fixture.state_root.clone()),
                ..StatusOptions::default()
            },
        )
        .unwrap_or_else(|error| panic!("{projection:?}: status pending mutations: {error:?}"));
        assert_eq!(status.summary.dirty, 3, "{projection:?}: {status:#?}");
        assert_status_issue(
            &status,
            "Home/Draft.md",
            "pending_virtual_create",
            &format!("{projection:?}"),
        );
        assert_status_issue(
            &status,
            "Home/Renamed Child/page.md",
            "pending_virtual_rename",
            &format!("{projection:?}"),
        );
        assert_status_issue(
            &status,
            "Home/Child Delete/page.md",
            "pending_virtual_delete",
            &format!("{projection:?}"),
        );

        let create_diff = run_diff_with_state_root(
            &store,
            fixture.root.join("Home/Draft.md"),
            Some(&fixture.state_root),
        )
        .unwrap_or_else(|error| panic!("{projection:?}: diff pending create: {error:?}"));
        assert!(create_diff.ok, "{projection:?}: {create_diff:#?}");
        assert_eq!(
            create_diff.action, "confirm_plan",
            "{projection:?}: {create_diff:#?}"
        );
        let create_plan = create_diff
            .plan
            .as_ref()
            .unwrap_or_else(|| panic!("{projection:?}: missing pending create plan"));
        assert_eq!(
            create_plan.summary.entities_created, 1,
            "{projection:?}: {create_plan:#?}"
        );

        let rename_diff = run_diff_with_state_root(
            &store,
            fixture.root.join("Home/Renamed Child/page.md"),
            Some(&fixture.state_root),
        )
        .unwrap_or_else(|error| panic!("{projection:?}: diff pending rename: {error:?}"));
        assert!(rename_diff.ok, "{projection:?}: {rename_diff:#?}");
        assert_eq!(
            rename_diff.action, "confirm_plan",
            "{projection:?}: {rename_diff:#?}"
        );
        let rename_plan = rename_diff
            .plan
            .as_ref()
            .unwrap_or_else(|| panic!("{projection:?}: missing pending rename plan"));
        assert_eq!(
            rename_plan.summary.entities_moved, 1,
            "{projection:?}: {rename_plan:#?}"
        );
        assert_eq!(
            rename_plan.affected_entities,
            vec!["child-rename".to_string()],
            "{projection:?}: {rename_plan:#?}"
        );

        let delete_diff = run_diff_with_state_root(
            &store,
            fixture.root.join("Home/Child Delete/page.md"),
            Some(&fixture.state_root),
        )
        .unwrap_or_else(|error| panic!("{projection:?}: diff pending delete: {error:?}"));
        assert!(delete_diff.ok, "{projection:?}: {delete_diff:#?}");
        assert_eq!(
            delete_diff.action, "confirm_plan",
            "{projection:?}: {delete_diff:#?}"
        );
        let delete_plan = delete_diff
            .plan
            .as_ref()
            .unwrap_or_else(|| panic!("{projection:?}: missing pending delete plan"));
        assert_eq!(
            delete_plan.summary.entities_archived, 1,
            "{projection:?}: {delete_plan:#?}"
        );
        assert_eq!(
            delete_plan.affected_entities,
            vec!["child-delete".to_string()],
            "{projection:?}: {delete_plan:#?}"
        );
        assert_eq!(
            store
                .list_virtual_mutations(&fixture.mount_id)
                .expect("list pending mutations")
                .len(),
            3,
            "{projection:?}: pending create deletion should collapse without leaving a mutation"
        );
    }
}

#[test]
fn virtual_page_directory_move_push_then_move_back_push_reconciles() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_page_and_blocks(
        page("page-child", "Child"),
        vec![paragraph_block("child-block", "Child body.")],
    ));
    let connector = NotionConnector::with_api(
        NotionConfig::default().with_root_page_id(RemoteId::new("page-home")),
        api.clone(),
    );
    mount_virtual_workspace(&fixture, &mut store, "page-home");
    seed_virtual_page(
        &mut store,
        &fixture,
        "page-home",
        "Home",
        "Home/page.md",
        "Home body.",
    );
    seed_virtual_page(
        &mut store,
        &fixture,
        "page-archive",
        "Archive",
        "Archive/page.md",
        "Archive body.",
    );
    seed_virtual_page(
        &mut store,
        &fixture,
        "page-child",
        "Child",
        "Home/Child/page.md",
        "Child body.",
    );
    let content_root = fixture.content_root();

    let moved_to_archive = rename_virtual_fs_item(
        &mut store,
        &content_root,
        &fixture.mount_id,
        "children:page-child",
        "children:page-archive",
        "Child",
    )
    .expect("move child page under archive");
    assert_eq!(moved_to_archive.item.path, "Archive/Child");
    let archive_page_path = fixture.root.join("Archive/Child/page.md");
    let archive_diff =
        run_diff_with_state_root(&store, &archive_page_path, Some(&fixture.state_root))
            .expect("diff pending move to archive");
    assert!(archive_diff.ok, "{archive_diff:#?}");
    let archive_plan = archive_diff.plan.as_ref().expect("archive move plan");
    assert_eq!(archive_plan.summary.entities_moved, 1, "{archive_plan:#?}");
    assert_eq!(
        archive_plan.operations,
        vec![PushOperationOutput::MoveEntity {
            entity_id: "page-child".to_string(),
            new_parent_id: "page-archive".to_string(),
            new_parent_kind: EntityKind::Page,
            new_title: "Child".to_string(),
            projected_path: "Archive/Child/page.md".to_string(),
        }],
        "{archive_plan:#?}"
    );

    let archive_push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &archive_page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push move to archive");
    assert!(archive_push.ok, "{archive_push:#?}");
    assert_eq!(archive_push.action, "reconciled", "{archive_push:#?}");
    assert_eq!(
        archive_push.changed_remote_ids,
        vec!["page-child".to_string()]
    );
    assert!(
        store
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list mutations after archive move")
            .is_empty(),
        "archive move push should clear pending mutations"
    );
    assert!(
        !content_root.join("Home/Child/page.md").exists(),
        "archive move should leave no stale page.md at the original location"
    );

    let moved_home = rename_virtual_fs_item(
        &mut store,
        &content_root,
        &fixture.mount_id,
        "children:page-child",
        "children:page-home",
        "Child",
    )
    .expect("move child page back home");
    assert_eq!(moved_home.item.path, "Home/Child");
    let home_page_path = fixture.root.join("Home/Child/page.md");
    let home_diff = run_diff_with_state_root(&store, &home_page_path, Some(&fixture.state_root))
        .expect("diff pending move back home");
    assert!(home_diff.ok, "{home_diff:#?}");
    let home_plan = home_diff.plan.as_ref().expect("home move plan");
    assert_eq!(home_plan.summary.entities_moved, 1, "{home_plan:#?}");
    assert_eq!(
        home_plan.operations,
        vec![PushOperationOutput::MoveEntity {
            entity_id: "page-child".to_string(),
            new_parent_id: "page-home".to_string(),
            new_parent_kind: EntityKind::Page,
            new_title: "Child".to_string(),
            projected_path: "Home/Child/page.md".to_string(),
        }],
        "{home_plan:#?}"
    );

    let home_push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &home_page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push move back home");
    assert!(home_push.ok, "{home_push:#?}");
    assert_eq!(home_push.action, "reconciled", "{home_push:#?}");
    assert_eq!(home_push.changed_remote_ids, vec!["page-child".to_string()]);
    assert!(
        store
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list mutations after home move")
            .is_empty(),
        "home move push should clear pending mutations"
    );
    assert!(
        !content_root.join("Archive/Child/page.md").exists(),
        "move back should leave no stale page.md at the intermediate location"
    );
    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new("page-child"))
        .expect("get moved child entity")
        .expect("moved child entity");
    assert_eq!(entity.path, PathBuf::from("Home/Child/page.md"));
    assert_eq!(entity.hydration, HydrationState::Hydrated);
    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(home_page_path),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("status after move back");
    assert!(clean_status.clean, "{clean_status:#?}");

    let move_calls = api
        .calls
        .lock()
        .expect("calls")
        .iter()
        .filter_map(|call| match call {
            WriteCall::MovePage { page_id, parent } => Some((page_id.clone(), parent.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        move_calls,
        vec![
            (
                "page-child".to_string(),
                json!({
                    "type": "page_id",
                    "page_id": "page-archive",
                }),
            ),
            (
                "page-child".to_string(),
                json!({
                    "type": "page_id",
                    "page_id": "page-home",
                }),
            ),
        ]
    );
}

#[test]
fn virtual_page_directory_move_rename_permutations_push_each_step_return_to_start() {
    #[derive(Clone, Copy, Debug)]
    struct Step {
        name: &'static str,
        parent_id: &'static str,
        parent_identifier: &'static str,
        parent_title: &'static str,
        title: &'static str,
    }

    impl Step {
        fn dir_path(&self) -> String {
            format!("{}/{}", self.parent_title, self.title)
        }

        fn page_path(&self) -> String {
            format!("{}/page.md", self.dir_path())
        }
    }

    fn page_with_page_parent(id: &str, title: &str, parent_id: &str) -> PageDto {
        let mut page = page(id, title);
        page.parent = Some(ParentDto {
            kind: "page_id".to_string(),
            page_id: Some(parent_id.to_string()),
            ..ParentDto::default()
        });
        page
    }

    let home_child = Step {
        name: "home child",
        parent_id: "page-home",
        parent_identifier: "children:page-home",
        parent_title: "Home",
        title: "Child",
    };
    let home_renamed = Step {
        name: "home renamed",
        parent_id: "page-home",
        parent_identifier: "children:page-home",
        parent_title: "Home",
        title: "Renamed Child",
    };
    let archive_child = Step {
        name: "archive child",
        parent_id: "page-archive",
        parent_identifier: "children:page-archive",
        parent_title: "Archive",
        title: "Child",
    };
    let archive_renamed = Step {
        name: "archive renamed",
        parent_id: "page-archive",
        parent_identifier: "children:page-archive",
        parent_title: "Archive",
        title: "Renamed Child",
    };

    let cases = vec![
        (
            "rename_move_move_back_rename_back",
            vec![home_renamed, archive_renamed, home_renamed, home_child],
        ),
        (
            "rename_move_rename_back_move_back",
            vec![home_renamed, archive_renamed, archive_child, home_child],
        ),
        (
            "move_rename_move_back_rename_back",
            vec![archive_child, archive_renamed, home_renamed, home_child],
        ),
        (
            "move_rename_rename_back_move_back",
            vec![archive_child, archive_renamed, archive_child, home_child],
        ),
        (
            "combined_move_rename_combined_restore",
            vec![archive_renamed, home_child],
        ),
        (
            "combined_move_rename_move_back_rename_back",
            vec![archive_renamed, home_renamed, home_child],
        ),
        (
            "combined_move_rename_rename_back_move_back",
            vec![archive_renamed, archive_child, home_child],
        ),
        (
            "rename_move_combined_restore",
            vec![home_renamed, archive_renamed, home_child],
        ),
        (
            "move_rename_combined_restore",
            vec![archive_child, archive_renamed, home_child],
        ),
    ];

    for (case_name, steps) in cases {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        let api = Arc::new(MutableNotionApi::with_page_and_blocks(
            page_with_page_parent("page-child", "Child", "page-home"),
            vec![paragraph_block("child-block", "Child body.")],
        ));
        let connector = NotionConnector::with_api(
            NotionConfig::default().with_root_page_id(RemoteId::new("page-home")),
            api.clone(),
        );
        mount_virtual_workspace(&fixture, &mut store, "page-home");
        seed_virtual_page(
            &mut store,
            &fixture,
            "page-home",
            "Home",
            "Home/page.md",
            "Home body.",
        );
        seed_virtual_page(
            &mut store,
            &fixture,
            "page-archive",
            "Archive",
            "Archive/page.md",
            "Archive body.",
        );
        seed_virtual_page(
            &mut store,
            &fixture,
            "page-child",
            "Child",
            "Home/Child/page.md",
            "Child body.",
        );

        let content_root = fixture.content_root();
        let mut current_page_path = PathBuf::from(home_child.page_path());
        let mut current_parent_id = home_child.parent_id;
        let mut expected_move_parents = Vec::new();

        for (step_index, step) in steps.iter().enumerate() {
            let previous_page_path = current_page_path.clone();
            let moved = rename_virtual_fs_item(
                &mut store,
                &content_root,
                &fixture.mount_id,
                "children:page-child",
                step.parent_identifier,
                step.title,
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{case_name}: step {step_index} {} rename/move: {error:?}",
                    step.name
                )
            });
            assert_eq!(
                moved.identifier, "children:page-child",
                "{case_name}: step {step_index} {}",
                step.name
            );
            assert_eq!(
                moved.item.path,
                step.dir_path(),
                "{case_name}: step {step_index} {}",
                step.name
            );
            assert_eq!(
                moved.item.filename, step.title,
                "{case_name}: step {step_index} {}",
                step.name
            );

            let projected_page_path = PathBuf::from(step.page_path());
            let visible_page_path = fixture.root.join(&projected_page_path);
            let diff =
                run_diff_with_state_root(&store, &visible_page_path, Some(&fixture.state_root))
                    .unwrap_or_else(|error| {
                        panic!(
                            "{case_name}: step {step_index} {} diff: {error:?}",
                            step.name
                        )
                    });
            assert!(diff.ok, "{case_name}: step {step_index} {diff:#?}");
            assert_eq!(
                diff.action, "confirm_plan",
                "{case_name}: step {step_index} {diff:#?}"
            );
            let plan = diff.plan.as_ref().unwrap_or_else(|| {
                panic!("{case_name}: step {step_index} {} missing plan", step.name)
            });
            assert_eq!(
                plan.summary.entities_moved, 1,
                "{case_name}: step {step_index} {plan:#?}"
            );
            assert_eq!(
                plan.operations,
                vec![PushOperationOutput::MoveEntity {
                    entity_id: "page-child".to_string(),
                    new_parent_id: step.parent_id.to_string(),
                    new_parent_kind: EntityKind::Page,
                    new_title: step.title.to_string(),
                    projected_path: step.page_path(),
                }],
                "{case_name}: step {step_index} {plan:#?}"
            );

            let push = run_push_with_daemon_at_state_root(
                &mut store,
                &connector,
                &visible_page_path,
                PushOptions {
                    assume_yes: true,
                    confirm_dangerous: false,
                },
                Some(&fixture.state_root),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{case_name}: step {step_index} {} push: {error:?}",
                    step.name
                )
            });
            assert!(push.ok, "{case_name}: step {step_index} {push:#?}");
            assert_eq!(
                push.action, "reconciled",
                "{case_name}: step {step_index} {push:#?}"
            );
            assert_eq!(
                push.changed_remote_ids,
                vec!["page-child".to_string()],
                "{case_name}: step {step_index} {push:#?}"
            );
            assert!(
                store
                    .list_virtual_mutations(&fixture.mount_id)
                    .expect("list mutations after move/rename push")
                    .is_empty(),
                "{case_name}: step {step_index} pending virtual mutations should clear"
            );
            assert!(
                content_root.join(&projected_page_path).exists(),
                "{case_name}: step {step_index} projected page should exist at {}",
                projected_page_path.display()
            );
            if previous_page_path != projected_page_path {
                assert!(
                    !content_root.join(&previous_page_path).exists(),
                    "{case_name}: step {step_index} stale page should not remain at {}",
                    previous_page_path.display()
                );
            }

            let entity = store
                .get_entity(&fixture.mount_id, &RemoteId::new("page-child"))
                .expect("get moved/renamed child entity")
                .expect("moved/renamed child entity");
            assert_eq!(
                entity.path, projected_page_path,
                "{case_name}: step {step_index}"
            );
            assert_eq!(entity.title, step.title, "{case_name}: step {step_index}");
            assert_eq!(
                entity.hydration,
                HydrationState::Hydrated,
                "{case_name}: step {step_index}"
            );

            let clean_status = run_status(
                &store,
                StatusOptions {
                    path: Some(visible_page_path),
                    state_root: Some(fixture.state_root.clone()),
                    ..StatusOptions::default()
                },
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{case_name}: step {step_index} {} clean status: {error:?}",
                    step.name
                )
            });
            assert!(
                clean_status.clean,
                "{case_name}: step {step_index} {clean_status:#?}"
            );

            if current_parent_id != step.parent_id {
                expected_move_parents.push(step.parent_id.to_string());
            }
            current_parent_id = step.parent_id;
            current_page_path = projected_page_path;
        }

        let final_entity = store
            .get_entity(&fixture.mount_id, &RemoteId::new("page-child"))
            .expect("get final child entity")
            .expect("final child entity");
        assert_eq!(
            final_entity.path,
            PathBuf::from("Home/Child/page.md"),
            "{case_name}: final entity path"
        );
        assert_eq!(final_entity.title, "Child", "{case_name}: final title");

        let final_page = api.page.lock().expect("page").clone();
        let final_parent = final_page.parent.as_ref().expect("final page parent");
        assert_eq!(
            final_parent.page_id.as_deref(),
            Some("page-home"),
            "{case_name}: final remote parent"
        );
        let final_title = final_page
            .properties
            .get("title")
            .and_then(|property| property.title.first())
            .map(|text| text.plain_text.as_str());
        assert_eq!(
            final_title,
            Some("Child"),
            "{case_name}: final remote title"
        );

        let move_call_parents = api
            .calls
            .lock()
            .expect("calls")
            .iter()
            .filter_map(|call| match call {
                WriteCall::MovePage { page_id, parent } => {
                    assert_eq!(page_id, "page-child", "{case_name}: move page id");
                    parent
                        .get("page_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            move_call_parents, expected_move_parents,
            "{case_name}: move calls should match parent-changing steps"
        );
    }
}

#[test]
fn read_only_virtual_projection_modes_reject_local_mutations_without_dirty_state() {
    for projection in [
        ProjectionMode::MacosFileProvider,
        ProjectionMode::LinuxFuse,
        ProjectionMode::WindowsCloudFiles,
    ] {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        run_mount(
            &mut store,
            MountOptions {
                mount_id: fixture.mount_id.clone(),
                connector: "notion".to_string(),
                root: fixture.root.clone(),
                remote_root_id: Some(RemoteId::new("page-1")),
                connection_id: Some(ConnectionId::new("work")),
                read_only: true,
                projection: projection.clone(),
                settings_json: "{}".to_string(),
            },
        )
        .unwrap_or_else(|error| panic!("mount {projection:?} read-only projection: {error:?}"));
        seed_virtual_page(
            &mut store,
            &fixture,
            "page-1",
            "Home",
            "Home/page.md",
            "Home body.",
        );
        seed_virtual_page(
            &mut store,
            &fixture,
            "child-rename",
            "Child Rename",
            "Home/Child Rename/page.md",
            "Child rename body.",
        );
        seed_virtual_page(
            &mut store,
            &fixture,
            "child-delete",
            "Child Delete",
            "Home/Child Delete/page.md",
            "Child delete body.",
        );

        let content_root = fixture.content_root();
        let original_home = fs::read_to_string(content_root.join("Home/page.md"))
            .unwrap_or_else(|error| panic!("{projection:?}: read original virtual cache: {error}"));

        assert_error_contains(
            commit_virtual_fs_write(
                &mut store,
                &content_root,
                &fixture.mount_id,
                "page-1",
                b"---\ntitle: Home\n---\nChanged body.\n",
            ),
            "read-only mounts do not accept virtual filesystem writes",
            &format!("{projection:?}: write"),
        );
        assert_error_contains(
            create_virtual_fs_file(
                &mut store,
                &content_root,
                &fixture.mount_id,
                "children:page-1",
                "Draft.md",
            ),
            "read-only mounts do not accept virtual filesystem creates",
            &format!("{projection:?}: create file"),
        );
        assert_error_contains(
            create_virtual_fs_directory(
                &mut store,
                &content_root,
                &fixture.mount_id,
                "children:page-1",
                "Draft Page",
            ),
            "read-only mounts do not accept virtual filesystem creates",
            &format!("{projection:?}: create directory"),
        );
        assert_error_contains(
            rename_virtual_fs_item(
                &mut store,
                &content_root,
                &fixture.mount_id,
                "children:child-rename",
                "children:page-1",
                "Renamed Child",
            ),
            "read-only mounts do not accept virtual filesystem renames",
            &format!("{projection:?}: rename"),
        );
        assert_error_contains(
            trash_virtual_fs_item(
                &mut store,
                &content_root,
                &fixture.mount_id,
                "children:child-delete",
            ),
            "read-only mounts do not accept virtual filesystem deletes",
            &format!("{projection:?}: delete"),
        );

        assert_eq!(
            fs::read_to_string(content_root.join("Home/page.md"))
                .unwrap_or_else(|error| panic!("{projection:?}: read unchanged cache: {error}")),
            original_home,
            "{projection:?}: rejected write must leave daemon content cache unchanged"
        );
        assert!(
            !content_root.join("Home/Draft.md").exists(),
            "{projection:?}: rejected create must not write a pending draft file"
        );
        assert_eq!(
            store
                .list_virtual_mutations(&fixture.mount_id)
                .expect("list read-only virtual mutations")
                .len(),
            0,
            "{projection:?}: read-only operations must not record virtual mutations"
        );
        let child = store
            .get_entity(&fixture.mount_id, &RemoteId::new("child-rename"))
            .expect("get rename child")
            .expect("rename child entity");
        assert_eq!(
            child.path,
            PathBuf::from("Home/Child Rename/page.md"),
            "{projection:?}: rejected rename must leave entity path unchanged"
        );
        assert_eq!(
            child.hydration,
            HydrationState::Hydrated,
            "{projection:?}: rejected rename must not dirty the entity"
        );

        let status = run_status(
            &store,
            StatusOptions {
                path: Some(fixture.root.clone()),
                state_root: Some(fixture.state_root.clone()),
                ..StatusOptions::default()
            },
        )
        .unwrap_or_else(|error| {
            panic!("{projection:?}: status after read-only attempts: {error:?}")
        });
        assert!(status.clean, "{projection:?}: {status:#?}");
        assert_eq!(status.summary.clean, 3, "{projection:?}: {status:#?}");
        assert_eq!(status.summary.dirty, 0, "{projection:?}: {status:#?}");
        assert_eq!(
            status.summary.pending_local_changes, 0,
            "{projection:?}: {status:#?}"
        );
    }
}

#[test]
fn shared_virtual_projection_modes_root_lists_mount_points_and_statuses_all_mounts() {
    for projection in [
        ProjectionMode::MacosFileProvider,
        ProjectionMode::LinuxFuse,
        ProjectionMode::WindowsCloudFiles,
    ] {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        let shared_root = fixture.root.join("Locality");
        fs::create_dir_all(&shared_root).expect("create shared virtual root");
        let notion_mount_id = MountId::new("notion-main");
        let docs_mount_id = MountId::new("google-docs-main");

        for (mount_id, connector, mount_point, remote_root_id) in [
            (
                notion_mount_id.clone(),
                "notion",
                "notion-main",
                "notion-root",
            ),
            (
                docs_mount_id.clone(),
                "google-docs",
                "google-docs-main",
                "docs-root",
            ),
        ] {
            run_mount(
                &mut store,
                MountOptions {
                    mount_id,
                    connector: connector.to_string(),
                    root: shared_root.join(mount_point),
                    remote_root_id: Some(RemoteId::new(remote_root_id)),
                    connection_id: None,
                    read_only: false,
                    projection: projection.clone(),
                    settings_json: "{}".to_string(),
                },
            )
            .unwrap_or_else(|error| panic!("{projection:?}: mount {mount_point}: {error:?}"));
        }

        seed_virtual_page_for_mount(
            &mut store,
            &fixture.state_root,
            &notion_mount_id,
            "notion-root",
            "Notion Home",
            "Notion Home/page.md",
            "Notion body.",
        );
        seed_virtual_page_for_mount(
            &mut store,
            &fixture.state_root,
            &docs_mount_id,
            "docs-root",
            "Docs Home",
            "Docs Home/page.md",
            "Docs body.",
        );

        let docs_content_root = virtual_fs_content_root(&fixture.state_root, &docs_mount_id);
        let docs_draft = create_virtual_fs_file(
            &mut store,
            &docs_content_root,
            &docs_mount_id,
            "mount:google-docs-main",
            "Draft.md",
        )
        .unwrap_or_else(|error| {
            panic!(
                "{projection:?}: create Google Docs pending draft through virtual mount: {error}"
            )
        });
        commit_virtual_fs_write(
            &mut store,
            &docs_content_root,
            &docs_mount_id,
            &docs_draft.identifier,
            render_canonical_markdown(&CanonicalDocument::new(
                "title: Draft\n",
                "Draft body.\n".to_string(),
            ))
            .as_bytes(),
        )
        .unwrap_or_else(|error| {
            panic!("{projection:?}: write Google Docs pending draft through virtual mount: {error}")
        });

        let root_children =
            virtual_projection_root_children(&store, &shared_root, projection.clone())
                .unwrap_or_else(|error| {
                    panic!("{projection:?}: list shared virtual root children: {error}")
                });
        let visible_mount_points = root_children
            .children
            .iter()
            .map(|child| {
                let identifier = unwrap_identifier(&child.identifier).unwrap_or_else(|error| {
                    panic!("{projection:?}: unwrap shared identifier: {error}")
                });
                (
                    child.filename.clone(),
                    identifier.mount_id.as_str().to_string(),
                    identifier.daemon_identifier,
                    child.materialized_path.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            visible_mount_points,
            vec![
                (
                    "google-docs-main".to_string(),
                    "google-docs-main".to_string(),
                    "mount:google-docs-main".to_string(),
                    Some(shared_root.join("google-docs-main").display().to_string())
                ),
                (
                    "notion-main".to_string(),
                    "notion-main".to_string(),
                    "mount:notion-main".to_string(),
                    Some(shared_root.join("notion-main").display().to_string())
                ),
            ],
            "{projection:?}: {root_children:#?}"
        );

        let shared_status = run_status(
            &store,
            StatusOptions {
                path: Some(shared_root.clone()),
                state_root: Some(fixture.state_root.clone()),
                ..StatusOptions::default()
            },
        )
        .unwrap_or_else(|error| panic!("{projection:?}: status at shared virtual root: {error:?}"));
        assert_eq!(
            shared_status.mounts.len(),
            2,
            "{projection:?}: {shared_status:#?}"
        );
        assert_eq!(
            shared_status.summary.total, 3,
            "{projection:?}: {shared_status:#?}"
        );
        assert_eq!(
            shared_status.summary.clean, 2,
            "{projection:?}: {shared_status:#?}"
        );
        assert_eq!(
            shared_status.summary.dirty, 1,
            "{projection:?}: {shared_status:#?}"
        );
        assert_eq!(
            shared_status.summary.pending_local_changes, 1,
            "{projection:?}: {shared_status:#?}"
        );
        assert_status_issue(
            &shared_status,
            "Draft.md",
            "pending_virtual_create",
            &format!("{projection:?}: shared virtual root status"),
        );

        let notion_status = run_status(
            &store,
            StatusOptions {
                path: Some(shared_root.join("notion-main")),
                state_root: Some(fixture.state_root.clone()),
                ..StatusOptions::default()
            },
        )
        .unwrap_or_else(|error| {
            panic!("{projection:?}: status at one shared virtual mount point: {error:?}")
        });
        assert_eq!(
            notion_status.mounts.len(),
            1,
            "{projection:?}: {notion_status:#?}"
        );
        assert_eq!(notion_status.mounts[0].mount_id, "notion-main");
        assert_eq!(
            notion_status.summary.total, 1,
            "{projection:?}: {notion_status:#?}"
        );
        assert_eq!(
            notion_status.summary.clean, 1,
            "{projection:?}: {notion_status:#?}"
        );
        assert!(notion_status.clean, "{projection:?}: {notion_status:#?}");
    }
}

#[test]
fn virtual_projection_modes_pull_page_directory_hydrates_target_and_descendants() {
    for projection in [
        ProjectionMode::MacosFileProvider,
        ProjectionMode::LinuxFuse,
        ProjectionMode::WindowsCloudFiles,
    ] {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        let source = NestedPagePullSource::new(&fixture.mount_id);
        run_mount(
            &mut store,
            MountOptions {
                mount_id: fixture.mount_id.clone(),
                connector: "notion".to_string(),
                root: fixture.root.clone(),
                remote_root_id: Some(RemoteId::new("root-page")),
                connection_id: Some(ConnectionId::new("work")),
                read_only: false,
                projection: projection.clone(),
                settings_json: "{}".to_string(),
            },
        )
        .unwrap_or_else(|error| {
            panic!("mount {projection:?} recursive page pull fixture: {error:?}")
        });

        let initial_pull = run_pull_with_state_root(
            &mut store,
            &source,
            &fixture.root,
            Some(&fixture.state_root),
        )
        .unwrap_or_else(|error| {
            panic!("{projection:?}: initial recursive page pull root: {error:?}")
        });
        assert!(initial_pull.ok, "{projection:?}: {initial_pull:#?}");

        let content_root = fixture.content_root();
        let target_directory = fixture.root.join("Roadmap").join("Design Notes");
        let target_page_path = content_root.join("Roadmap/Design Notes/page.md");
        assert!(
            !target_page_path.exists(),
            "{projection:?}: initial root pull should leave target page online-only"
        );

        let recursive_pull = run_pull_with_state_root(
            &mut store,
            &source,
            &target_directory,
            Some(&fixture.state_root),
        )
        .unwrap_or_else(|error| panic!("{projection:?}: recursive directory pull: {error:?}"));
        assert!(recursive_pull.ok, "{projection:?}: {recursive_pull:#?}");
        assert_eq!(
            recursive_pull.hydrated, 3,
            "{projection:?}: {recursive_pull:#?}"
        );
        assert_eq!(
            recursive_pull.enumerated, 2,
            "{projection:?}: recursive directory pull should enumerate nested descendants: {recursive_pull:#?}"
        );

        let target_entity = store
            .find_entity_by_path(
                &fixture.mount_id,
                &PathBuf::from("Roadmap/Design Notes/page.md"),
            )
            .unwrap_or_else(|error| panic!("{projection:?}: read target entity: {error:?}"))
            .unwrap_or_else(|| panic!("{projection:?}: missing target entity after pull"));
        assert_eq!(
            target_entity.hydration,
            HydrationState::Hydrated,
            "{projection:?}: pulling a page directory should hydrate that directory's own page.md"
        );
        assert!(
            target_page_path.exists(),
            "{projection:?}: pulling a page directory should materialize its own page.md"
        );
        assert!(
            fs::read_to_string(&target_page_path)
                .unwrap_or_else(|error| panic!("{projection:?}: read target page: {error}"))
                .contains("Design notes body."),
            "{projection:?}: hydrated target page should contain rendered content"
        );

        let appendix_path = content_root.join("Roadmap/Design Notes/Appendix/page.md");
        let further_reading_path =
            content_root.join("Roadmap/Design Notes/Appendix/Further Reading/page.md");
        assert!(
            appendix_path.exists(),
            "{projection:?}: recursive page-directory pull should hydrate child page.md files"
        );
        assert!(
            further_reading_path.exists(),
            "{projection:?}: recursive page-directory pull should hydrate nested descendant page.md files"
        );
        assert!(
            fs::read_to_string(&appendix_path)
                .unwrap_or_else(|error| panic!("{projection:?}: read appendix page: {error}"))
                .contains("Appendix body."),
            "{projection:?}: hydrated child page should contain rendered content"
        );
        assert!(
            fs::read_to_string(&further_reading_path)
                .unwrap_or_else(|error| panic!(
                    "{projection:?}: read further reading page: {error}"
                ))
                .contains("Further reading body."),
            "{projection:?}: hydrated nested descendant should contain rendered content"
        );
    }
}

#[test]
fn scheduled_pull_large_workspace_stubs_metadata_and_queues_only_root_hydration() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("root-page")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount scheduled pull budget workspace");

    let entries = scheduled_tree_entries(&fixture.mount_id, 32);
    let source = StaticScheduledPullSource::new(entries);
    let mounts = store.load_mounts().expect("load mounts");
    let strategy = DefaultFetchScheduleStrategy;
    let policy = HydrationPolicy::default();
    let mut scheduler = PullScheduler::new(Default::default());
    let tick = scheduler.tick().expect("scheduled pull tick");
    let mut queue = HydrationQueue::new();

    let report = reconcile_scheduled_pull(
        &mut store, &mut queue, &mounts, &tick, &source, &strategy, &policy,
    )
    .expect("scheduled pull budget reconcile");
    assert_eq!(source.enumeration_count(), 1);
    assert_eq!(report.mounts_polled, 1, "{report:#?}");
    assert_eq!(report.enumerated, 33, "{report:#?}");
    assert_eq!(report.stubbed, 33, "{report:#?}");
    assert_eq!(report.queued_hydrations, 1, "{report:#?}");

    let root_stub = fs::read_to_string(fixture.root.join("Root/page.md")).expect("root stub");
    assert!(
        root_stub.contains(CanonicalDocument::STUB_MARKER),
        "{root_stub}"
    );
    let child_stub =
        fs::read_to_string(fixture.root.join("Root/Child 17/page.md")).expect("child stub");
    assert!(
        child_stub.contains(CanonicalDocument::STUB_MARKER),
        "{child_stub}"
    );
    assert!(
        !child_stub.contains("Child 17 body"),
        "scheduled pull should write metadata stubs without hydrating child page bodies:\n{child_stub}"
    );

    let mut queued = Vec::new();
    while let Some(request) = queue.pop_ready() {
        queued.push(request);
    }
    assert_eq!(queued.len(), 1, "{queued:#?}");
    assert_eq!(queued[0].remote_id, RemoteId::new("root-page"));
    assert_eq!(queued[0].reason, HydrationReason::Policy);

    let child = store
        .get_entity(&fixture.mount_id, &RemoteId::new("child-17"))
        .expect("get scheduled child")
        .expect("scheduled child");
    assert_eq!(child.hydration, HydrationState::Stub);
}

#[test]
fn scheduled_pull_idle_ticks_do_not_enumerate_or_queue_duplicate_hydrations() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("root-page")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount scheduled pull idle workspace");

    let source = StaticScheduledPullSource::new(scheduled_tree_entries(&fixture.mount_id, 8));
    let mounts = store.load_mounts().expect("load mounts");
    let strategy = DefaultFetchScheduleStrategy;
    let policy = HydrationPolicy::default();
    let mut scheduler = PullScheduler::new(locality_core::pull::PullSchedulerConfig {
        active_interval: Duration::from_secs(10),
        cold_interval: Duration::from_secs(100),
        ..Default::default()
    });
    let mut queue = HydrationQueue::new();

    let initial_tick = scheduler.tick().expect("initial scheduled pull tick");
    assert!(!initial_tick.is_idle(), "{initial_tick:#?}");
    let initial_report = reconcile_scheduled_pull(
        &mut store,
        &mut queue,
        &mounts,
        &initial_tick,
        &source,
        &strategy,
        &policy,
    )
    .expect("initial scheduled pull reconcile");
    assert_eq!(source.enumeration_count(), 1);
    assert_eq!(initial_report.mounts_polled, 1, "{initial_report:#?}");
    assert_eq!(initial_report.enumerated, 9, "{initial_report:#?}");
    assert_eq!(initial_report.queued_hydrations, 1, "{initial_report:#?}");
    assert_eq!(queue.len(), 1);

    for tick_number in 1..=5 {
        let idle_tick = scheduler
            .advance_by(Duration::from_secs(1))
            .expect("idle scheduled pull tick");
        assert!(
            idle_tick.is_idle(),
            "tick {tick_number} should be idle: {idle_tick:#?}"
        );

        let idle_report = reconcile_scheduled_pull(
            &mut store, &mut queue, &mounts, &idle_tick, &source, &strategy, &policy,
        )
        .expect("idle scheduled pull reconcile");
        assert_eq!(source.enumeration_count(), 1);
        assert_eq!(idle_report.mounts_checked, 1, "{idle_report:#?}");
        assert_eq!(idle_report.mounts_polled, 0, "{idle_report:#?}");
        assert_eq!(idle_report.enumerated, 0, "{idle_report:#?}");
        assert_eq!(idle_report.queued_hydrations, 0, "{idle_report:#?}");
        assert_eq!(queue.len(), 1, "tick {tick_number}: {idle_report:#?}");
    }

    let request = queue.pop_ready().expect("queued root hydration");
    assert_eq!(request.remote_id, RemoteId::new("root-page"));
    assert_eq!(request.reason, HydrationReason::Policy);
    assert!(queue.pop_ready().is_none());
}

#[test]
fn scheduled_pull_hour_of_ticks_keeps_api_polls_to_interval_budget() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("root-page")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount scheduled pull hour budget workspace");

    let source = StaticScheduledPullSource::new(scheduled_tree_entries(&fixture.mount_id, 8));
    let mounts = store.load_mounts().expect("load mounts");
    let strategy = DefaultFetchScheduleStrategy;
    let policy = HydrationPolicy::default();
    let mut scheduler = PullScheduler::new(locality_core::pull::PullSchedulerConfig {
        active_interval: Duration::from_secs(60),
        cold_interval: Duration::from_secs(600),
        ..Default::default()
    });
    let mut queue = HydrationQueue::new();

    let initial_tick = scheduler.tick().expect("initial scheduled pull tick");
    let initial = reconcile_scheduled_pull(
        &mut store,
        &mut queue,
        &mounts,
        &initial_tick,
        &source,
        &strategy,
        &policy,
    )
    .expect("initial scheduled pull reconcile");
    assert_eq!(initial.mounts_polled, 1, "{initial:#?}");
    assert_eq!(initial.enumerated, 9, "{initial:#?}");
    assert_eq!(queue.len(), 1, "{initial:#?}");

    let mut non_idle_ticks = 1_u64;
    for elapsed_second in 1..=3600 {
        let tick = scheduler
            .advance_by(Duration::from_secs(1))
            .expect("scheduled pull budget tick");
        let report = reconcile_scheduled_pull(
            &mut store, &mut queue, &mounts, &tick, &source, &strategy, &policy,
        )
        .expect("scheduled pull budget reconcile");

        if tick.is_idle() {
            assert_eq!(
                report.enumerated, 0,
                "idle tick at second {elapsed_second} should not enumerate: {report:#?}"
            );
        } else {
            non_idle_ticks += 1;
            assert_eq!(
                report.mounts_polled, 1,
                "due tick at second {elapsed_second}: {report:#?}"
            );
        }
        assert_eq!(
            queue.len(),
            1,
            "scheduled pull should merge duplicate hydration work at second {elapsed_second}: {report:#?}"
        );
    }

    assert_eq!(non_idle_ticks, 61);
    assert_eq!(
        source.enumeration_count(),
        non_idle_ticks,
        "one mounted API enumeration should run per due scheduler tick"
    );
}

#[test]
fn daemon_runtime_polling_scheduler_keeps_wall_clock_api_budget() {
    let fixture = E2eFixture::new();
    let runner = WallClockScheduledPullRunner::default();
    let mut config = localityd::DaemonConfig {
        state_root: fixture.state_root.clone(),
        runtime_tick_interval: Duration::from_millis(5),
        ..Default::default()
    };
    config.pull_scheduler.mode = locality_core::pull::PullMode::Polling;
    config.pull_scheduler.active_interval = Duration::from_millis(80);
    config.pull_scheduler.cold_interval = Duration::from_secs(60);

    let runtime = localityd::runtime::DaemonRuntime::spawn_with_runner(config, runner.clone())
        .expect("spawn polling daemon runtime");
    let first = runner.wait_for_scheduled_count(1, Duration::from_secs(1));
    assert_eq!(first, 1, "daemon scheduler should run an initial poll");

    let scheduled = runner.wait_for_scheduled_count(3, Duration::from_secs(1));
    runtime.shutdown();

    assert_eq!(
        scheduled, 3,
        "daemon scheduler should reach exactly the initial poll plus two active-interval polls before the condition is satisfied"
    );
}

#[test]
fn workspace_virtual_projection_modes_freshness_prioritizes_hot_work_and_caps_cold_budget() {
    for projection in [
        ProjectionMode::MacosFileProvider,
        ProjectionMode::LinuxFuse,
        ProjectionMode::WindowsCloudFiles,
    ] {
        let fixture = E2eFixture::new();
        let mut store = InMemoryStateStore::new();
        run_mount(
            &mut store,
            MountOptions {
                mount_id: fixture.mount_id.clone(),
                connector: "notion".to_string(),
                root: fixture.root.clone(),
                remote_root_id: None,
                connection_id: Some(ConnectionId::new("work")),
                read_only: false,
                projection: projection.clone(),
                settings_json: "{}".to_string(),
            },
        )
        .expect("mount workspace virtual freshness fixture");
        let mounts = store
            .load_mounts()
            .expect("load workspace freshness mounts");
        let mount_id = fixture.mount_id.clone();

        save_workspace_freshness_page(
            &mut store,
            &mount_id,
            "dirty-page",
            "Dirty Page",
            "Dirty Page/page.md",
            HydrationState::Dirty,
        );
        save_workspace_freshness_page(
            &mut store,
            &mount_id,
            "conflicted-page",
            "Conflicted Page",
            "Conflicted Page/page.md",
            HydrationState::Conflicted,
        );
        save_workspace_freshness_page(
            &mut store,
            &mount_id,
            "hot-open-page",
            "Hot Open Page",
            "Hot Open Page/page.md",
            HydrationState::Hydrated,
        );
        save_workspace_freshness_page(
            &mut store,
            &mount_id,
            "remote-hint-page",
            "Remote Hint Page",
            "Remote Hint Page/page.md",
            HydrationState::Hydrated,
        );
        save_workspace_freshness_page(
            &mut store,
            &mount_id,
            "stub-page",
            "Stub Page",
            "Stub Page/page.md",
            HydrationState::Stub,
        );
        save_workspace_freshness_page(
            &mut store,
            &mount_id,
            "virtual-page",
            "Virtual Page",
            "Virtual Page/page.md",
            HydrationState::Virtual,
        );
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    mount_id.clone(),
                    RemoteId::new("hot-open-page"),
                    FreshnessTier::Hot,
                )
                .opened_at("unix_ms:18446744073709551615")
                .checked_at("unix_ms:1"),
            )
            .expect("save hot freshness");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    mount_id.clone(),
                    RemoteId::new("remote-hint-page"),
                    FreshnessTier::Warm,
                )
                .remote_hint_pending(true)
                .checked_at("unix_ms:2"),
            )
            .expect("save remote-hint freshness");

        for index in 0..130 {
            let remote_id = format!("warm-page-{index:03}");
            save_workspace_freshness_page(
                &mut store,
                &mount_id,
                &remote_id,
                format!("Warm Page {index:03}"),
                format!("Warm Page {index:03}/page.md"),
                HydrationState::Hydrated,
            );
            store
                .save_freshness_state(
                    FreshnessStateRecord::new(
                        mount_id.clone(),
                        RemoteId::new(remote_id),
                        FreshnessTier::Warm,
                    )
                    .checked_at(format!("unix_ms:{}", index + 10)),
                )
                .expect("save warm freshness");
        }

        let active_jobs = workspace_virtual_freshness_jobs(
            &store,
            &mounts,
            &PullSchedulerTick {
                poll_active: true,
                poll_cold: false,
            },
        )
        .expect("active workspace freshness jobs");
        let active_facts = active_jobs
            .iter()
            .map(|job| {
                (
                    job.remote_id.as_ref().expect("active remote id").clone(),
                    (job.reason.clone(), job.tier.clone()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(active_facts.len(), 4, "{projection:?}: {active_jobs:#?}");
        assert_eq!(
            active_facts.get(&RemoteId::new("dirty-page")),
            Some(&(ChangeHintKind::LocalEdited, FreshnessTier::Hot)),
            "{projection:?}: {active_jobs:#?}"
        );
        assert_eq!(
            active_facts.get(&RemoteId::new("conflicted-page")),
            Some(&(ChangeHintKind::LocalEdited, FreshnessTier::Hot)),
            "{projection:?}: {active_jobs:#?}"
        );
        assert_eq!(
            active_facts.get(&RemoteId::new("hot-open-page")),
            Some(&(ChangeHintKind::FileOpened, FreshnessTier::Hot)),
            "{projection:?}: {active_jobs:#?}"
        );
        assert_eq!(
            active_facts.get(&RemoteId::new("remote-hint-page")),
            Some(&(ChangeHintKind::RemoteMaybeChanged, FreshnessTier::Hot)),
            "{projection:?}: {active_jobs:#?}"
        );
        assert!(
            !active_facts.contains_key(&RemoteId::new("stub-page"))
                && !active_facts.contains_key(&RemoteId::new("virtual-page")),
            "{projection:?}: stub and virtual pages should not consume active freshness budget: {active_jobs:#?}"
        );

        let cold_jobs = workspace_virtual_freshness_jobs(
            &store,
            &mounts,
            &PullSchedulerTick {
                poll_active: true,
                poll_cold: true,
            },
        )
        .expect("cold workspace freshness jobs");
        assert_eq!(cold_jobs.len(), 100, "{projection:?}: {cold_jobs:#?}");
        assert!(
            cold_jobs
                .iter()
                .all(|job| job.kind == SyncJobKind::ObserveEntity),
            "{projection:?}: {cold_jobs:#?}"
        );
        let cold_ids = cold_jobs
            .iter()
            .map(|job| job.remote_id.as_ref().expect("cold remote id").clone())
            .collect::<Vec<_>>();
        for expected in [
            "dirty-page",
            "conflicted-page",
            "hot-open-page",
            "remote-hint-page",
            "warm-page-000",
        ] {
            assert!(
                cold_ids.contains(&RemoteId::new(expected)),
                "{projection:?}: missing {expected} from bounded cold jobs: {cold_jobs:#?}"
            );
        }
        assert!(
            !cold_ids.contains(&RemoteId::new("warm-page-129")),
            "{projection:?}: newest warm page should be outside the capped cold budget: {cold_jobs:#?}"
        );
        assert!(
            !cold_ids.contains(&RemoteId::new("stub-page"))
                && !cold_ids.contains(&RemoteId::new("virtual-page")),
            "{projection:?}: stub and virtual pages should not consume cold freshness budget: {cold_jobs:#?}"
        );
    }
}

#[test]
fn notion_remote_observation_surfaces_remote_update_without_hydrating_blocks() {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let api = Arc::new(MutableNotionApi::with_blocks(vec![paragraph_block(
        "block-1",
        "Remote observation should not fetch this body.",
    )]));
    let connector = NotionConnector::with_api(NotionConfig::default(), api.clone());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new("page-1")),
            connection_id: Some(ConnectionId::new("work")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount remote observation fixture");
    store
        .save_connection(ConnectionRecord {
            connection_id: ConnectionId::new("work"),
            profile_id: None,
            connector: "notion".to_string(),
            display_name: "Work".to_string(),
            account_label: None,
            workspace_id: None,
            workspace_name: None,
            auth_kind: "token".to_string(),
            secret_ref: "connection:work".to_string(),
            scopes: vec![],
            capabilities_json: notion_capabilities_json_for_live_test(),
            status: "active".to_string(),
            created_at: "0".to_string(),
            updated_at: "0".to_string(),
            expires_at: None,
        })
        .expect("seed active connection for access-aware search");
    let pull = run_pull(&mut store, &connector, &fixture.root).expect("pull observation fixture");
    assert!(pull.ok, "{pull:#?}");
    let page_path = fixture.page_file();
    let pulled = fs::read_to_string(&page_path).expect("read observed page");
    assert!(
        pulled.contains("Remote observation should not fetch this body."),
        "{pulled}"
    );

    let block_children_before = api.block_children_count();
    replace_mutable_page_title_and_version(
        &api,
        "Observed Remote Rename",
        "2026-06-10T00:00:02.000Z",
    );
    let observation = connector
        .observe(ObserveRequest {
            mount_id: fixture.mount_id.clone(),
            remote_id: RemoteId::new("page-1"),
        })
        .expect("observe remote metadata");
    assert_eq!(api.block_children_count(), block_children_before);
    assert_eq!(observation.title, "Observed Remote Rename");
    assert_eq!(
        observation.remote_version,
        Some(RemoteVersion::new("2026-06-10T00:00:02.000Z"))
    );

    let observed_at = "unix_ms:42";
    let mut record = RemoteObservationRecord::new(
        observation.mount_id.clone(),
        observation.remote_id.clone(),
        observation.kind.clone(),
        observation.title.clone(),
        observation.projected_path.clone(),
        observed_at,
    )
    .deleted(observation.deleted)
    .with_raw_metadata_json(observation.raw_metadata_json.clone());
    if let Some(parent_remote_id) = observation.parent_remote_id.clone() {
        record = record.with_parent(parent_remote_id);
    }
    if let Some(remote_version) = observation.remote_version.clone() {
        record = record.with_remote_version(remote_version);
    }
    store
        .save_remote_observation(record)
        .expect("save observed remote metadata");
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("page-1"),
                FreshnessTier::Hot,
            )
            .checked_at(observed_at)
            .remote_hint_pending(true),
        )
        .expect("save freshness state");

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path),
            ..StatusOptions::default()
        },
    )
    .expect("status with observed remote update");
    assert!(!status.clean, "{status:#?}");
    assert_eq!(status.summary.remote_update_available, 1, "{status:#?}");
    let entry = status
        .mounts
        .iter()
        .flat_map(|mount| mount.entries.iter())
        .find(|entry| entry.path.ends_with("page.md"))
        .expect("observed status entry");
    assert_eq!(entry.state.as_str(), "clean", "{entry:#?}");
    assert_eq!(
        entry.sync_state.as_str(),
        "remote_update_available",
        "{entry:#?}"
    );
    assert_eq!(
        entry.remote.remote_tree_version.as_deref(),
        Some("2026-06-10T00:00:02.000Z"),
        "{entry:#?}"
    );
    assert_eq!(
        entry.remote.remote_tree_observed_at.as_deref(),
        Some(observed_at),
        "{entry:#?}"
    );
    assert!(
        entry
            .issues
            .iter()
            .any(|issue| issue.code == "remote_changed"),
        "{entry:#?}"
    );

    let search = run_search(&store, SearchOptions::new("Observed Remote Rename"))
        .expect("search observed remote title");
    let result = search
        .results
        .iter()
        .find(|result| result.remote_id == "page-1")
        .expect("search result from observed metadata");
    assert_eq!(result.state, "remote_update_available", "{result:#?}");
    assert!(!result.safety.agent_readable, "{result:#?}");
    assert_eq!(
        result.remote.observed_title.as_deref(),
        Some("Observed Remote Rename"),
        "{result:#?}"
    );
    assert_eq!(
        result.remote.observed_path.as_deref(),
        Some("Observed Remote Rename/page.md"),
        "{result:#?}"
    );
}

#[test]
fn credential_secret_parsing_accepts_plain_token_and_oauth_json() {
    assert_eq!(
        notion_access_token_from_secret("secret_plain_token").expect("plain token"),
        "secret_plain_token"
    );

    let oauth_secret = json!({
        "kind": "oauth",
        "access_token": "secret_oauth_token",
        "refresh_token": null,
        "token_type": "bearer",
        "oauth_client_id": "test-client",
        "oauth_client_secret": null,
        "oauth_broker_url": "https://broker.example.test",
        "workspace_id": "workspace-id",
        "workspace_name": "Workspace",
        "bot_id": "bot-id",
        "refresh_token_handle": "refresh-handle",
        "acquired_at": 123,
        "expires_at": null
    });
    assert_eq!(
        notion_access_token_from_secret(&oauth_secret.to_string()).expect("oauth token"),
        "secret_oauth_token"
    );
}

#[test]
fn live_parent_preflight_rejects_archived_or_trashed_page() {
    let mut archived = page("archived-parent", "Archived Parent");
    archived.archived = true;
    let archived_error = std::panic::catch_unwind(|| {
        ensure_live_parent_page_is_writable(&archived, "archived-parent");
    })
    .expect_err("archived parent should be rejected");
    let archived_message = panic_payload_message(archived_error);
    assert!(
        archived_message.contains(LIVE_PARENT_ENV),
        "{archived_message}"
    );
    assert!(
        archived_message.contains("archived-parent"),
        "{archived_message}"
    );
    assert!(
        archived_message.contains("archived or in trash"),
        "{archived_message}"
    );

    let mut trashed = page("trashed-parent", "Trashed Parent");
    trashed.in_trash = true;
    let trashed_error = std::panic::catch_unwind(|| {
        ensure_live_parent_page_is_writable(&trashed, "trashed-parent");
    })
    .expect_err("trashed parent should be rejected");
    let trashed_message = panic_payload_message(trashed_error);
    assert!(
        trashed_message.contains(LIVE_PARENT_ENV),
        "{trashed_message}"
    );
    assert!(
        trashed_message.contains("trashed-parent"),
        "{trashed_message}"
    );
    assert!(
        trashed_message.contains("archived or in trash"),
        "{trashed_message}"
    );
}

#[test]
fn live_parent_preflight_accepts_active_page() {
    let active = page("active-parent", "Active Parent");
    ensure_live_parent_page_is_writable(&active, "active-parent");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_scratch_page_mount_edit_push_verifies_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let marker = format!("Locality live mounted edit {}", unique_suffix());
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality mounted e2e {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": [
                    {
                        "type": "text",
                        "text": {
                            "content": "Original paragraph created by the mounted Locality live e2e test."
                        }
                    }
                ]
            }
        })],
    );

    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let connector = NotionConnector::new(live_notion_config());

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(scratch.id.clone())),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount");
    run_pull(&mut store, &connector, &fixture.root).expect("pull");
    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read pulled page");
    assert!(original.contains("Original paragraph created by the mounted Locality live e2e test."));
    fs::write(&page_path, format!("{original}\n\n{marker}\n")).expect("write local edit");

    let diff = run_diff(&store, &page_path).expect("diff");
    let plan = diff.plan.as_ref().expect("plan");
    assert_eq!(diff.action, "confirm_plan");
    assert!(plan.summary.blocks_created >= 1, "{plan:#?}");

    let dirty_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("dirty status");
    assert!(!dirty_status.clean, "{dirty_status:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push");
    assert!(push.ok, "{push:#?}");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let verified = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new(scratch.id),
        })
        .expect("verify fetch");
    let verified_render = connector
        .render_native_entity_for_path(&verified, &page_path)
        .expect("verify render");
    assert!(
        verified_render.document.body.contains(&marker),
        "{}",
        verified_render.document.body
    );
}

#[test]
#[ignore = "requires Notion credentials in ~/.loc credentials and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cli_binary_uses_stored_credential_and_pushes_scratch_page() {
    let env = LiveEnv::from_env();
    let source_connection_id =
        std::env::var(LIVE_CONNECTION_ENV).unwrap_or_else(|_| "notion-default".to_string());
    let stored_secret =
        live_notion_secret_from_default_store(&source_connection_id).expect("stored credential");
    let access_token =
        notion_access_token_from_secret(&stored_secret).expect("stored access token");
    let api = HttpNotionApi::new(NotionConfig::default().with_token(access_token.clone()));
    let mut cleanup = LiveCleanup::new(api);
    let marker = format!("Locality live CLI binary edit {}", unique_suffix());
    let base = "Original paragraph for CLI binary live e2e.";
    let scratch_title = format!("Locality live CLI binary {}", unique_suffix());
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &scratch_title,
        vec![paragraph_child(base)],
    );

    let fixture = E2eFixture::new();
    let loc = env!("CARGO_BIN_EXE_loc");
    let connection_id = ConnectionId::new("stored-live-notion");
    seed_cli_live_connection(&fixture.state_root, &connection_id, &stored_secret);

    let connections =
        loc_json_ok(loc_command(loc, &fixture.state_root).args(["connections", "--json"]));
    assert_eq!(connections.value["ok"], true, "{connections:#?}");
    assert!(
        !connections.stdout.contains(&access_token),
        "connections JSON leaked the Notion access token"
    );
    assert!(
        !connections.stdout.contains("secret_ref"),
        "connections JSON leaked credential storage internals"
    );

    let profiles = loc_json_ok(loc_command(loc, &fixture.state_root).args(["profiles", "--json"]));
    assert_eq!(profiles.value["ok"], true, "{profiles:#?}");
    assert!(
        profiles.value["profiles"]
            .as_array()
            .expect("profiles")
            .iter()
            .any(|profile| profile["profile_id"] == DEFAULT_NOTION_PROFILE_ID),
        "{profiles:#?}"
    );
    assert!(
        !profiles.stdout.contains(&access_token),
        "profiles JSON leaked the Notion access token"
    );
    assert!(
        !profiles.stdout.contains("secret_ref"),
        "profiles JSON leaked credential storage internals"
    );

    let connection_show = loc_json_ok(loc_command(loc, &fixture.state_root).args([
        "connection",
        "show",
        connection_id.as_str(),
        "--json",
    ]));
    assert_eq!(connection_show.value["ok"], true, "{connection_show:#?}");
    assert_eq!(
        connection_show.value["connection"]["connection_id"],
        connection_id.as_str(),
        "{connection_show:#?}"
    );
    assert_eq!(
        connection_show.value["connection"]["status"], "active",
        "{connection_show:#?}"
    );
    assert!(
        !connection_show.stdout.contains(&access_token),
        "connection show JSON leaked the Notion access token"
    );
    assert!(
        !connection_show.stdout.contains("secret_ref"),
        "connection show JSON leaked credential storage internals"
    );

    let mount = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("mount")
            .arg("notion")
            .arg(&fixture.root)
            .arg("--root-page")
            .arg(&scratch.id)
            .arg("--connection")
            .arg(connection_id.as_str())
            .arg("--mount-id")
            .arg(fixture.mount_id.as_str())
            .arg("--projection")
            .arg("plain-files")
            .arg("--json"),
    );
    assert_eq!(mount.value["ok"], true, "{mount:#?}");

    let pull = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("pull")
            .arg(&fixture.root)
            .arg("--json"),
    );
    assert_eq!(pull.value["ok"], true, "{pull:#?}");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read CLI-pulled page");
    assert!(original.contains(base), "{original}");

    let info = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("info")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(info.value["ok"], true, "{info:#?}");
    assert_eq!(info.value["command"], "info", "{info:#?}");
    assert_eq!(
        info.value["mount"]["mount_id"],
        fixture.mount_id.as_str(),
        "{info:#?}"
    );
    assert_eq!(info.value["subject"]["role"], "page_file", "{info:#?}");
    assert_eq!(info.value["subject"]["source"], "Notion page", "{info:#?}");
    assert_eq!(
        compact_notion_id(
            info.value["subject"]["entity"]["entity_id"]
                .as_str()
                .expect("info entity id")
        ),
        compact_notion_id(&scratch.id),
        "{info:#?}"
    );
    assert!(
        !info.stdout.contains(&access_token),
        "info JSON leaked the Notion access token"
    );

    let doctor = loc_json_ok(loc_command(loc, &fixture.state_root).args(["doctor", "--json"]));
    assert_eq!(doctor.value["ok"], true, "{doctor:#?}");
    assert_eq!(doctor.value["command"], "doctor", "{doctor:#?}");
    assert_ne!(doctor.value["status"], "error", "{doctor:#?}");
    assert!(
        !doctor.value["findings"]
            .as_array()
            .expect("doctor findings")
            .iter()
            .any(|finding| finding["severity"] == "error"),
        "{doctor:#?}"
    );
    let doctor_connection = doctor.value["connections"]
        .as_array()
        .expect("doctor connections")
        .iter()
        .find(|connection| connection["connection_id"] == connection_id.as_str())
        .expect("doctor connection");
    assert_eq!(doctor_connection["status"], "active", "{doctor:#?}");
    assert_eq!(doctor_connection["profile_status"], "ok", "{doctor:#?}");
    assert_eq!(doctor_connection["credential_status"], "ok", "{doctor:#?}");
    let doctor_mount = doctor.value["mounts"]
        .as_array()
        .expect("doctor mounts")
        .iter()
        .find(|mount| mount["mount_id"] == fixture.mount_id.as_str())
        .expect("doctor mount");
    assert_eq!(doctor_mount["root_exists"], true, "{doctor:#?}");
    assert_eq!(
        doctor_mount["connection_id"],
        connection_id.as_str(),
        "{doctor:#?}"
    );
    assert!(
        !doctor.stdout.contains(&access_token),
        "doctor JSON leaked the Notion access token"
    );

    let search = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("search")
            .arg(&scratch_title)
            .arg("--connector")
            .arg("notion")
            .arg("--json"),
    );
    let search_results = search.value["results"].as_array().expect("search results");
    let found_scratch = search_results.iter().any(|result| {
        compact_notion_id(
            result["remote_id"]
                .as_str()
                .expect("search result remote_id"),
        ) == compact_notion_id(&scratch.id)
            && result["absolute_path"] == page_path.display().to_string()
    });
    if !found_scratch {
        let debug_store =
            SqliteStateStore::open(fixture.state_root.clone()).expect("open debug search state");
        let debug_entities = debug_store
            .list_entities(&fixture.mount_id)
            .expect("debug list entities")
            .into_iter()
            .map(|entity| {
                format!(
                    "{}|{}|{}",
                    entity.title,
                    entity.path.display(),
                    compact_notion_id(&entity.remote_id.0)
                )
            })
            .collect::<Vec<_>>();
        panic!("{search:#?}\nindexed entities: {debug_entities:#?}");
    }

    let clean_inspect = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("inspect")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(
        clean_inspect.value["explanation"]["state"], "all_synced",
        "{clean_inspect:#?}"
    );
    assert_eq!(
        clean_inspect.value["explanation"]["action"], "none",
        "{clean_inspect:#?}"
    );

    fs::write(&page_path, original.replace(base, &marker)).expect("write CLI live edit");

    let dirty = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("status")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(dirty.value["clean"], false, "{dirty:#?}");

    let dirty_inspect = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("inspect")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(
        dirty_inspect.value["explanation"]["state"], "local_changed_only",
        "{dirty_inspect:#?}"
    );
    assert_eq!(
        dirty_inspect.value["explanation"]["action"], "push_local_changes",
        "{dirty_inspect:#?}"
    );

    let diff = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("diff")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(diff.value["action"], "confirm_plan", "{diff:#?}");

    let push = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("push")
            .arg(&page_path)
            .arg("-y")
            .arg("--json"),
    );
    assert_eq!(push.value["ok"], true, "{push:#?}");
    assert_eq!(push.value["action"], "reconciled", "{push:#?}");
    let push_id = push.value["push_id"]
        .as_str()
        .expect("push report push_id")
        .to_string();

    let clean = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("status")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(clean.value["clean"], true, "{clean:#?}");

    let connector = NotionConnector::new(NotionConfig::default().with_token(access_token.clone()));
    let pushed_remote = render_live_page(&connector, &scratch.id, &page_path);
    assert!(pushed_remote.contains(&marker), "{pushed_remote}");

    let restore_probe = format!("Local restore-only probe {}", unique_suffix());
    let pushed_local = fs::read_to_string(&page_path).expect("read pushed CLI file");
    fs::write(&page_path, format!("{pushed_local}\n\n{restore_probe}\n"))
        .expect("write restore probe");
    let restore_dirty = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("status")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(restore_dirty.value["clean"], false, "{restore_dirty:#?}");

    let restore = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("restore")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(restore.value["ok"], true, "{restore:#?}");
    assert_eq!(restore.value["action"], "restored", "{restore:#?}");
    assert_eq!(
        restore.value["mount_id"],
        fixture.mount_id.as_str(),
        "{restore:#?}"
    );
    assert_eq!(
        compact_notion_id(restore.value["entity_id"].as_str().expect("restore entity")),
        compact_notion_id(&scratch.id),
        "{restore:#?}"
    );

    let restore_clean = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("status")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(restore_clean.value["clean"], true, "{restore_clean:#?}");
    let restored_from_shadow = fs::read_to_string(&page_path).expect("read restored CLI file");
    assert!(
        restored_from_shadow.contains(&marker),
        "{restored_from_shadow}"
    );
    assert!(
        !restored_from_shadow.contains(&restore_probe),
        "{restored_from_shadow}"
    );

    let log = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("log")
            .arg(&page_path)
            .arg("--json"),
    );
    let log_entries = log.value["entries"].as_array().expect("log entries");
    assert_eq!(log_entries.len(), 1, "{log:#?}");
    assert_eq!(log_entries[0]["push_id"], push_id, "{log:#?}");
    assert_eq!(log_entries[0]["status"], "reconciled", "{log:#?}");

    let undo = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("undo")
            .arg(&push_id)
            .arg("--json"),
    );
    assert_eq!(undo.value["ok"], true, "{undo:#?}");
    assert_eq!(undo.value["action"], "reverse_applied", "{undo:#?}");
    assert_eq!(undo.value["status"], "reverted", "{undo:#?}");

    let restored_remote = render_live_page(&connector, &scratch.id, &page_path);
    assert!(restored_remote.contains(base), "{restored_remote}");
    assert!(
        !restored_remote.contains(&marker),
        "undo should restore remote content:\n{restored_remote}"
    );
    let restored_local = fs::read_to_string(&page_path).expect("read undo-restored CLI file");
    assert!(restored_local.contains(base), "{restored_local}");
    assert!(!restored_local.contains(&marker), "{restored_local}");

    let reverted_log = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("log")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(
        reverted_log.value["entries"][0]["push_id"], push_id,
        "{reverted_log:#?}"
    );
    assert_eq!(
        reverted_log.value["entries"][0]["status"], "reverted",
        "{reverted_log:#?}"
    );

    let disconnect = loc_json_ok(loc_command(loc, &fixture.state_root).args([
        "disconnect",
        connection_id.as_str(),
        "--json",
    ]));
    assert_eq!(disconnect.value["ok"], true, "{disconnect:#?}");
    assert_eq!(disconnect.value["command"], "disconnect", "{disconnect:#?}");
    assert_eq!(
        disconnect.value["connection_id"],
        connection_id.as_str(),
        "{disconnect:#?}"
    );
    assert_eq!(disconnect.value["status"], "revoked", "{disconnect:#?}");
    assert!(
        !disconnect.stdout.contains(&access_token),
        "disconnect JSON leaked the Notion access token"
    );
    assert!(
        live_notion_secret_from_state_root(&fixture.state_root, connection_id.as_str()).is_err(),
        "disconnect should delete the stored credential"
    );

    let revoked_show = loc_json_ok(loc_command(loc, &fixture.state_root).args([
        "connection",
        "show",
        connection_id.as_str(),
        "--json",
    ]));
    assert_eq!(
        revoked_show.value["connection"]["status"], "revoked",
        "{revoked_show:#?}"
    );
    assert!(
        !revoked_show.stdout.contains(&access_token),
        "revoked connection show JSON leaked the Notion access token"
    );
    assert!(
        !revoked_show.stdout.contains("secret_ref"),
        "revoked connection show JSON leaked credential storage internals"
    );
}

#[test]
#[ignore = "requires Notion credentials in ~/.loc credentials and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cli_live_mode_enables_file_auto_save_without_immediate_push() {
    let env = LiveEnv::from_env();
    let source_connection_id =
        std::env::var(LIVE_CONNECTION_ENV).unwrap_or_else(|_| "notion-default".to_string());
    let stored_secret =
        live_notion_secret_from_default_store(&source_connection_id).expect("stored credential");
    let access_token =
        notion_access_token_from_secret(&stored_secret).expect("stored access token");
    let api = HttpNotionApi::new(NotionConfig::default().with_token(access_token.clone()));
    let mut cleanup = LiveCleanup::new(api);
    let base = "Original paragraph for CLI Live Mode e2e.";
    let marker = format!("Locality live CLI Live Mode edit {}", unique_suffix());
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live CLI Live Mode {}", unique_suffix()),
        vec![paragraph_child(base)],
    );

    let fixture = E2eFixture::new();
    let loc = env!("CARGO_BIN_EXE_loc");
    let connection_id = ConnectionId::new("stored-live-notion-live-mode");
    seed_cli_live_connection(&fixture.state_root, &connection_id, &stored_secret);

    let mount = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("mount")
            .arg("notion")
            .arg(&fixture.root)
            .arg("--root-page")
            .arg(&scratch.id)
            .arg("--connection")
            .arg(connection_id.as_str())
            .arg("--mount-id")
            .arg(fixture.mount_id.as_str())
            .arg("--projection")
            .arg("plain-files")
            .arg("--json"),
    );
    assert_eq!(mount.value["ok"], true, "{mount:#?}");

    let pull = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("pull")
            .arg(&fixture.root)
            .arg("--json"),
    );
    assert_eq!(pull.value["ok"], true, "{pull:#?}");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read CLI Live Mode page");
    assert!(original.contains(base), "{original}");
    fs::write(&page_path, original.replace(base, &marker)).expect("write Live Mode local edit");

    let enable = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("live-mode")
            .arg("on")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(enable.value["ok"], true, "{enable:#?}");
    assert_eq!(enable.value["command"], "live_mode", "{enable:#?}");
    assert_eq!(enable.value["action"], "enabled", "{enable:#?}");
    assert_eq!(
        compact_notion_id(enable.value["remote_id"].as_str().expect("remote id")),
        compact_notion_id(&scratch.id),
        "{enable:#?}"
    );
    assert_eq!(enable.value["enabled"], true, "{enable:#?}");
    assert_eq!(enable.value["state"], "active", "{enable:#?}");

    let relative_path = page_path
        .strip_prefix(&fixture.root)
        .expect("page is under mount root")
        .to_path_buf();
    let state_after_enable =
        SqliteStateStore::open(fixture.state_root.clone()).expect("open live-mode state");
    let enrollment = state_after_enable
        .get_auto_save_enrollment(&fixture.mount_id, &relative_path)
        .expect("load live enrollment")
        .expect("live enrollment");
    assert!(enrollment.enabled, "{enrollment:#?}");
    assert_eq!(enrollment.state, AutoSaveState::Active);
    assert_eq!(enrollment.origin, AutoSaveOrigin::UserEnabled);
    assert_eq!(enrollment.last_reason, None);
    assert_eq!(
        compact_notion_id(
            enrollment
                .remote_id
                .as_ref()
                .expect("enrollment remote id")
                .as_str()
        ),
        compact_notion_id(&scratch.id)
    );
    drop(state_after_enable);

    let connector = NotionConnector::new(NotionConfig::default().with_token(access_token.clone()));
    let remote_after_enable = render_live_page(&connector, &scratch.id, &page_path);
    assert!(
        remote_after_enable.contains(base),
        "enabling file Live Mode must not push existing local edits:\n{remote_after_enable}"
    );
    assert!(
        !remote_after_enable.contains(&marker),
        "enabling file Live Mode must not write the marker remotely:\n{remote_after_enable}"
    );

    let mut store =
        SqliteStateStore::open(fixture.state_root.clone()).expect("open live auto-save state");
    let auto_save = execute_auto_save_push_job_with_content_root(
        &mut store,
        PushJob {
            target_path: page_path.clone(),
            assume_yes: false,
            confirm_dangerous: false,
        },
        &connector,
        Some(&fixture.state_root),
    )
    .expect("execute live auto-save push");
    assert_eq!(auto_save.action, PushJobAction::Reconciled);
    assert!(auto_save.error.is_none(), "{auto_save:#?}");

    let remote_after_auto_save = render_live_page(&connector, &scratch.id, &page_path);
    assert!(
        remote_after_auto_save.contains(&marker),
        "auto-save should push marker to Notion:\n{remote_after_auto_save}"
    );

    let disable = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("live-mode")
            .arg("off")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(disable.value["ok"], true, "{disable:#?}");
    assert_eq!(disable.value["action"], "disabled", "{disable:#?}");
    assert_eq!(disable.value["enabled"], false, "{disable:#?}");
}

#[test]
#[ignore = "requires Notion credentials in ~/.loc credentials and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cli_binary_read_only_mount_blocks_push_without_remote_write() {
    let env = LiveEnv::from_env();
    let source_connection_id =
        std::env::var(LIVE_CONNECTION_ENV).unwrap_or_else(|_| "notion-default".to_string());
    let stored_secret =
        live_notion_secret_from_default_store(&source_connection_id).expect("stored credential");
    let access_token =
        notion_access_token_from_secret(&stored_secret).expect("stored access token");
    let api = HttpNotionApi::new(NotionConfig::default().with_token(access_token.clone()));
    let mut cleanup = LiveCleanup::new(api);
    let base = "Original paragraph for read-only CLI live e2e.";
    let marker = format!("Locality read-only blocked edit {}", unique_suffix());
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live read-only CLI {}", unique_suffix()),
        vec![paragraph_child(base)],
    );

    let fixture = E2eFixture::new();
    let loc = env!("CARGO_BIN_EXE_loc");
    let connection_id = ConnectionId::new("stored-live-notion-read-only");
    seed_cli_live_connection(&fixture.state_root, &connection_id, &stored_secret);

    let mount = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("mount")
            .arg("notion")
            .arg(&fixture.root)
            .arg("--root-page")
            .arg(&scratch.id)
            .arg("--connection")
            .arg(connection_id.as_str())
            .arg("--mount-id")
            .arg(fixture.mount_id.as_str())
            .arg("--projection")
            .arg("plain-files")
            .arg("--read-only")
            .arg("--json"),
    );
    assert_eq!(mount.value["ok"], true, "{mount:#?}");
    assert_eq!(mount.value["read_only"], true, "{mount:#?}");

    let pull = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("pull")
            .arg(&fixture.root)
            .arg("--json"),
    );
    assert_eq!(pull.value["ok"], true, "{pull:#?}");

    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read read-only CLI page");
    assert!(original.contains(base), "{original}");
    fs::write(&page_path, original.replace(base, &marker)).expect("write read-only local edit");

    let info = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("info")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(info.value["mount"]["read_only"], true, "{info:#?}");

    let diff = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("diff")
            .arg(&page_path)
            .arg("--json"),
    );
    assert_eq!(diff.value["action"], "read_only_blocked", "{diff:#?}");
    assert_eq!(
        compact_notion_id(diff.value["entity_id"].as_str().expect("diff entity id")),
        compact_notion_id(&scratch.id),
        "{diff:#?}"
    );

    let push = loc_json_with_exit(
        loc_command(loc, &fixture.state_root)
            .arg("push")
            .arg(&page_path)
            .arg("-y")
            .arg("--json"),
        4,
    );
    assert_eq!(push.value["ok"], false, "{push:#?}");
    assert_eq!(push.value["action"], "read_only_blocked", "{push:#?}");
    assert_eq!(push.value["push_id"], Value::Null, "{push:#?}");
    assert_eq!(push.value["journal_status"], Value::Null, "{push:#?}");
    assert_eq!(push.value["apply_effect_count"], 0, "{push:#?}");

    let connector = NotionConnector::new(NotionConfig::default().with_token(access_token));
    let remote = render_live_page(&connector, &scratch.id, &page_path);
    assert!(remote.contains(base), "{remote}");
    assert!(
        !remote.contains(&marker),
        "read-only push should not mutate Notion:\n{remote}"
    );

    let log = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("log")
            .arg(&page_path)
            .arg("--json"),
    );
    assert!(
        log.value["entries"]
            .as_array()
            .expect("read-only log entries")
            .is_empty(),
        "{log:#?}"
    );
}

#[test]
#[ignore = "requires Notion credentials in ~/.loc credentials and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cli_create_page_on_virtual_mount_pushes_child_page() {
    let env = LiveEnv::from_env();
    let source_connection_id =
        std::env::var(LIVE_CONNECTION_ENV).unwrap_or_else(|_| "notion-default".to_string());
    let stored_secret =
        live_notion_secret_from_default_store(&source_connection_id).expect("stored credential");
    let access_token =
        notion_access_token_from_secret(&stored_secret).expect("stored access token");
    let api = HttpNotionApi::new(NotionConfig::default().with_token(access_token.clone()));
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live virtual create parent {}", unique_suffix()),
        vec![paragraph_child("Parent body before virtual create.")],
    );

    let fixture = E2eFixture::new();
    let loc = env!("CARGO_BIN_EXE_loc");
    let connection_id = ConnectionId::new("stored-live-notion-virtual-create");
    seed_cli_live_connection(&fixture.state_root, &connection_id, &stored_secret);

    let mount = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("mount")
            .arg("notion")
            .arg(&fixture.root)
            .arg("--root-page")
            .arg(&scratch.id)
            .arg("--connection")
            .arg(connection_id.as_str())
            .arg("--mount-id")
            .arg(fixture.mount_id.as_str())
            .arg("--projection")
            .arg("plain-files")
            .arg("--json"),
    );
    assert_eq!(mount.value["ok"], true, "{mount:#?}");

    let pull = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("pull")
            .arg(&fixture.root)
            .arg("--json"),
    );
    assert_eq!(pull.value["ok"], true, "{pull:#?}");

    {
        let mut store =
            SqliteStateStore::open(fixture.state_root.clone()).expect("open live CLI state");
        let mut mount = store
            .get_mount(&fixture.mount_id)
            .expect("get mount")
            .expect("mounted workspace");
        mount.projection = ProjectionMode::MacosFileProvider;
        store.save_mount(mount).expect("save virtual projection");
    }

    let parent_page_path = fixture.page_file();
    let parent_dir = parent_page_path
        .parent()
        .expect("parent page directory")
        .to_path_buf();
    set_directory_readonly(&parent_dir, true);

    let child_title = format!("Locality live virtual child {}", unique_suffix());
    let mut create_command = loc_command(loc, &fixture.state_root);
    let output = create_command
        .arg("create")
        .arg("page")
        .arg("--title")
        .arg(&child_title)
        .arg("--parent")
        .arg(&parent_dir)
        .arg("--json")
        .output()
        .expect("loc create page");
    set_directory_readonly(&parent_dir, false);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "loc create page failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let create_value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("failed to parse create JSON: {error}\n{stdout}"));
    assert_eq!(create_value["ok"], true, "{create_value:#?}");
    let child_page_path = PathBuf::from(
        create_value["path"]
            .as_str()
            .expect("created page path in JSON"),
    );
    assert!(
        !child_page_path.exists(),
        "virtual create should not write into the visible provider projection"
    );

    let relative_child_path = child_page_path
        .strip_prefix(&fixture.root)
        .expect("created page under fixture root")
        .to_path_buf();
    let cached_body = format!(
        "---\ntitle: \"{child_title}\"\n---\n# Created through loc create\n\nCreated through loc create on a virtual Notion mount.\n"
    );
    {
        let store = SqliteStateStore::open(fixture.state_root.clone())
            .expect("open state for virtual create");
        let mutation = store
            .find_virtual_mutation_by_path(&fixture.mount_id, &relative_child_path)
            .expect("find virtual create mutation")
            .expect("pending virtual create");
        assert_eq!(
            mutation
                .parent_remote_id
                .as_ref()
                .map(|id| compact_notion_id(id.as_str())),
            Some(compact_notion_id(&scratch.id)),
            "{mutation:#?}"
        );
        let content_path = mutation.content_path.expect("pending create content path");
        fs::write(content_path, cached_body).expect("write pending virtual content");
    }

    let status = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("status")
            .arg(&child_page_path)
            .arg("--json"),
    );
    assert_eq!(status.value["clean"], false, "{status:#?}");

    let diff = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("diff")
            .arg(&child_page_path)
            .arg("--json"),
    );
    assert_eq!(diff.value["action"], "confirm_plan", "{diff:#?}");

    let push = loc_json_ok(
        loc_command(loc, &fixture.state_root)
            .arg("push")
            .arg(&child_page_path)
            .arg("-y")
            .arg("--json"),
    );
    assert_eq!(push.value["ok"], true, "{push:#?}");
    assert_eq!(push.value["action"], "reconciled", "{push:#?}");
    let created_page_id = push.value["changed_remote_ids"]
        .as_array()
        .expect("changed remote ids")
        .iter()
        .filter_map(Value::as_str)
        .find(|id| compact_notion_id(id) != compact_notion_id(&scratch.id))
        .expect("created child page id")
        .to_string();
    cleanup.block_ids.push(created_page_id.clone());

    let connector = NotionConnector::new(NotionConfig::default().with_token(access_token));
    let child_markdown = render_live_markdown(&connector, &created_page_id, &child_page_path);
    assert!(child_markdown.contains(&format!("title: \"{child_title}\"")));
    assert!(
        child_markdown.contains("Created through loc create on a virtual Notion mount."),
        "{child_markdown}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_block_type_replace_pushes_and_reconciles_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let original_text = "Replace block paragraph original.";
    let untouched_text = "Paragraph after replacement should remain.";
    let replacement_text = "Replacement bullet from live replace";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live replace block {}", unique_suffix()),
        vec![
            paragraph_child(original_text),
            paragraph_child(untouched_text),
        ],
    );
    let original_block_id = first_live_child_block_id(&cleanup.api, &scratch.id);
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);

    fs::write(
        &page_path,
        original.replace(original_text, &format!("- {replacement_text}")),
    )
    .expect("write live replace edit");
    let diff = run_diff(&store, &page_path).expect("diff live replace edit");
    let plan = diff.plan.as_ref().expect("replace plan");
    assert_eq!(diff.action, "confirm_plan");
    assert_eq!(plan.summary.blocks_replaced, 1, "{plan:#?}");
    assert_eq!(plan.summary.blocks_updated, 0, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live replace edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.journal_status.as_deref(), Some("reconciled"));
    assert_eq!(push.apply_effect_count, 2);

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean replace status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let verified = render_live_page(&connector, &scratch.id, &page_path);
    assert!(
        verified.contains(&format!("- {replacement_text}")),
        "{verified}"
    );
    assert!(verified.contains(untouched_text), "{verified}");
    assert!(
        !verified.contains(original_text),
        "replacement should archive the old paragraph:\n{verified}"
    );
    let after = live_block_snapshot(&connector, &scratch.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after replace");
    assert_eq!(first["block"]["type"], "bulleted_list_item");
    assert_ne!(first["block"]["id"], original_block_id);
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_directive_block_move_pushes_and_reconciles_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let anchor_text = "Move anchor paragraph.";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live directive move {}", unique_suffix()),
        vec![
            paragraph_child(anchor_text),
            json!({
                "object": "block",
                "type": "table_of_contents",
                "table_of_contents": { "color": "default" }
            }),
        ],
    );
    let connector = NotionConnector::new(live_notion_config());
    let before = live_block_snapshot(&connector, &scratch.id);
    let table_of_contents_id = before
        .as_array()
        .and_then(|blocks| {
            blocks.iter().find_map(|entry| {
                (entry["block"]["type"] == "table_of_contents")
                    .then(|| entry["block"]["id"].as_str())
                    .flatten()
            })
        })
        .expect("table_of_contents block id")
        .to_string();
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);
    let directive_line = original
        .lines()
        .find(|line| line.contains(&table_of_contents_id))
        .expect("table_of_contents directive line");
    let original_order = format!("{anchor_text}\n\n{directive_line}\n");
    assert!(original.contains(&original_order), "{original}");
    fs::write(
        &page_path,
        original.replace(
            &original_order,
            &format!("{directive_line}\n\n{anchor_text}\n\n"),
        ),
    )
    .expect("write live directive move");

    let diff = run_diff(&store, &page_path).expect("diff live directive move");
    let plan = diff.plan.as_ref().expect("directive move plan");
    assert_eq!(diff.action, "confirm_plan");
    assert_eq!(plan.summary.blocks_created, 0, "{plan:#?}");
    assert_eq!(plan.summary.blocks_moved, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live directive move");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.journal_status.as_deref(), Some("reconciled"));
    assert_eq!(push.apply_effect_count, 2);

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean directive move status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let after = live_block_snapshot(&connector, &scratch.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after move");
    assert_ne!(first["block"]["id"], table_of_contents_id);
    assert_eq!(first["block"]["type"], "table_of_contents");

    let verified = render_live_page(&connector, &scratch.id, &page_path);
    let directive_index = verified
        .lines()
        .position(|line| line.contains("type=table_of_contents"))
        .expect("reconciled directive line");
    let anchor_index = verified
        .lines()
        .position(|line| line == anchor_text)
        .expect("reconciled anchor paragraph");
    assert!(directive_index < anchor_index, "{verified}");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_directive_block_move_undo_restores_remote_order() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let anchor_text = "Undo move anchor paragraph.";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live directive move undo {}", unique_suffix()),
        vec![
            paragraph_child(anchor_text),
            json!({
                "object": "block",
                "type": "table_of_contents",
                "table_of_contents": { "color": "default" }
            }),
        ],
    );
    let connector = NotionConnector::new(live_notion_config());
    let before = live_block_snapshot(&connector, &scratch.id);
    let table_of_contents_id = before
        .as_array()
        .and_then(|blocks| {
            blocks.iter().find_map(|entry| {
                (entry["block"]["type"] == "table_of_contents")
                    .then(|| entry["block"]["id"].as_str())
                    .flatten()
            })
        })
        .expect("table_of_contents block id")
        .to_string();
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);
    let directive_line = original
        .lines()
        .find(|line| line.contains(&table_of_contents_id))
        .expect("table_of_contents directive line");
    let original_order = format!("{anchor_text}\n\n{directive_line}\n");
    assert!(original.contains(&original_order), "{original}");
    fs::write(
        &page_path,
        original.replace(
            &original_order,
            &format!("{directive_line}\n\n{anchor_text}\n\n"),
        ),
    )
    .expect("write live directive move for undo");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live directive move for undo");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    let push_id = push.push_id.clone().expect("push id");
    let moved = render_live_page(&connector, &scratch.id, &page_path);
    let moved_directive_index = moved
        .lines()
        .position(|line| line.contains("type=table_of_contents"))
        .expect("moved directive line");
    let moved_anchor_index = moved
        .lines()
        .position(|line| line == anchor_text)
        .expect("moved anchor paragraph");
    assert!(moved_directive_index < moved_anchor_index, "{moved}");

    let mut undo_applier = ConnectorUndoApplier::new(&connector);
    let undo = run_undo_with_applier(&mut store, push_id.clone(), &mut undo_applier)
        .expect("undo live directive move");
    assert!(undo.ok, "{undo:#?}");
    assert_eq!(undo.action, "reverse_applied", "{undo:#?}");
    assert_eq!(undo.status, "reverted");
    assert_eq!(undo.changed_remote_ids, vec![scratch.id.clone()]);

    let restored = render_live_page(&connector, &scratch.id, &page_path);
    let restored_anchor_index = restored
        .lines()
        .position(|line| line == anchor_text)
        .expect("restored anchor paragraph");
    let restored_directive_index = restored
        .lines()
        .position(|line| line.contains("type=table_of_contents"))
        .expect("restored directive line");
    assert!(
        restored_anchor_index < restored_directive_index,
        "{restored}"
    );
    assert_eq!(
        restored
            .lines()
            .filter(|line| line.contains("type=table_of_contents"))
            .count(),
        1,
        "{restored}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_link_to_page_line_move_preserves_notion_block_type() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let target = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live link move target {}", unique_suffix()),
        vec![paragraph_child("Target for link_to_page move.")],
    );
    let anchor_text = "Anchor before link_to_page.";
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live link move source {}", unique_suffix()),
        vec![
            paragraph_child(anchor_text),
            json!({
                "object": "block",
                "type": "link_to_page",
                "link_to_page": { "type": "page_id", "page_id": target.id }
            }),
        ],
    );
    let connector = NotionConnector::new(live_notion_config());
    let before = live_block_snapshot(&connector, &source.id);
    let original_link_id = before
        .as_array()
        .and_then(|blocks| {
            blocks.iter().find_map(|entry| {
                (entry["block"]["type"] == "link_to_page")
                    .then(|| entry["block"]["id"].as_str())
                    .flatten()
            })
        })
        .expect("link_to_page block id")
        .to_string();
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let link_line = original
        .lines()
        .find(|line| {
            line.starts_with("[Linked page](") && line.contains(&compact_notion_id(&target.id))
        })
        .expect("rendered link_to_page line");
    let anchor_line = original
        .lines()
        .find(|line| line.replace("\\_", "_") == anchor_text)
        .expect("rendered anchor paragraph");
    let original_order = format!("{anchor_line}\n\n{link_line}\n");
    assert!(original.contains(&original_order), "{original}");
    fs::write(
        &page_path,
        original.replace(
            &original_order,
            &format!("{link_line}\n\n{anchor_line}\n\n"),
        ),
    )
    .expect("write live link_to_page move");

    let diff = run_diff(&store, &page_path).expect("diff live link_to_page move");
    let plan = diff.plan.as_ref().expect("link_to_page move plan");
    assert_eq!(diff.action, "confirm_plan");
    assert_eq!(plan.summary.blocks_created, 1, "{plan:#?}");
    assert_eq!(plan.summary.blocks_archived, 1, "{plan:#?}");
    assert_eq!(plan.summary.blocks_moved, 0, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live link_to_page move");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.apply_effect_count, 2);

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean link_to_page move status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after link_to_page move");
    assert_ne!(first["block"]["id"], original_link_id);
    assert_eq!(first["block"]["type"], "link_to_page");
    assert_eq!(
        compact_notion_id(
            first["block"]["link_to_page"]["page_id"]
                .as_str()
                .expect("moved link target page id")
        ),
        compact_notion_id(&target.id)
    );

    let verified = render_live_page(&connector, &source.id, &page_path);
    let link_index = verified
        .lines()
        .position(|line| {
            line.starts_with("[Linked page](") && line.contains(&compact_notion_id(&target.id))
        })
        .expect("reconciled link_to_page line");
    let anchor_index = verified
        .lines()
        .position(|line| line.replace("\\_", "_") == anchor_text)
        .expect("reconciled anchor paragraph");
    assert!(link_index < anchor_index, "{verified}");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_link_to_database_line_move_preserves_notion_block_type() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let target = cleanup.create_database(
        &env.parent_page_id,
        &format!(
            "Locality live database link move target {}",
            unique_suffix()
        ),
    );
    let anchor_text = "Anchor before link_to_database.";
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!(
            "Locality live database link move source {}",
            unique_suffix()
        ),
        vec![
            paragraph_child(anchor_text),
            json!({
                "object": "block",
                "type": "link_to_page",
                "link_to_page": { "type": "database_id", "database_id": target.id }
            }),
        ],
    );
    let connector = NotionConnector::new(live_notion_config());
    let before = live_block_snapshot(&connector, &source.id);
    let original_link_id = before
        .as_array()
        .and_then(|blocks| {
            blocks.iter().find_map(|entry| {
                (entry["block"]["type"] == "link_to_page")
                    .then(|| entry["block"]["id"].as_str())
                    .flatten()
            })
        })
        .expect("link_to_database block id")
        .to_string();
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let link_line = original
        .lines()
        .find(|line| {
            line.starts_with("[Linked database](") && line.contains(&compact_notion_id(&target.id))
        })
        .expect("rendered link_to_database line");
    let anchor_line = original
        .lines()
        .find(|line| line.replace("\\_", "_") == anchor_text)
        .expect("rendered anchor paragraph");
    let original_order = format!("{anchor_line}\n\n{link_line}\n");
    assert!(original.contains(&original_order), "{original}");
    fs::write(
        &page_path,
        original.replace(
            &original_order,
            &format!("{link_line}\n\n{anchor_line}\n\n"),
        ),
    )
    .expect("write live link_to_database move");

    let diff = run_diff(&store, &page_path).expect("diff live link_to_database move");
    let plan = diff.plan.as_ref().expect("link_to_database move plan");
    assert_eq!(diff.action, "confirm_plan");
    assert_eq!(plan.summary.blocks_created, 1, "{plan:#?}");
    assert_eq!(plan.summary.blocks_archived, 1, "{plan:#?}");
    assert_eq!(plan.summary.blocks_moved, 0, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live link_to_database move");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.apply_effect_count, 2);

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean link_to_database move status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after link_to_database move");
    assert_ne!(first["block"]["id"], original_link_id);
    assert_eq!(first["block"]["type"], "link_to_page");
    assert_eq!(
        compact_notion_id(
            first["block"]["link_to_page"]["database_id"]
                .as_str()
                .expect("moved link target database id")
        ),
        compact_notion_id(&target.id)
    );

    let verified = render_live_page(&connector, &source.id, &page_path);
    let link_index = verified
        .lines()
        .position(|line| {
            line.starts_with("[Linked database](") && line.contains(&compact_notion_id(&target.id))
        })
        .expect("reconciled link_to_database line");
    let anchor_index = verified
        .lines()
        .position(|line| line.replace("\\_", "_") == anchor_text)
        .expect("reconciled anchor paragraph");
    assert!(link_index < anchor_index, "{verified}");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_link_to_page_retarget_blocks_before_journaled_apply() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let original_target = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live link retarget original {}", unique_suffix()),
        vec![paragraph_child(
            "Original target for link_to_page retarget.",
        )],
    );
    let replacement_target = cleanup.create_page(
        &env.parent_page_id,
        &format!(
            "Locality live link retarget replacement {}",
            unique_suffix()
        ),
        vec![paragraph_child(
            "Replacement target for link_to_page retarget.",
        )],
    );
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live link retarget source {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "link_to_page",
            "link_to_page": { "type": "page_id", "page_id": original_target.id }
        })],
    );
    let connector = NotionConnector::new(live_notion_config());
    let before = live_block_snapshot(&connector, &source.id);
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let link_line = original
        .lines()
        .find(|line| {
            line.starts_with("[Linked page](")
                && line.contains(&compact_notion_id(&original_target.id))
        })
        .expect("rendered link_to_page line");
    let edited_link_line = link_line.replace(
        &compact_notion_id(&original_target.id),
        &compact_notion_id(&replacement_target.id),
    );
    assert_ne!(link_line, edited_link_line);
    fs::write(&page_path, original.replace(link_line, &edited_link_line))
        .expect("write live link_to_page retarget");

    let diff = run_diff(&store, &page_path).expect("diff live link_to_page retarget");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(
        diff.validation[0].code,
        "notion_link_to_page_retarget_unsupported"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live link_to_page retarget");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(store.list_journal().expect("journal").is_empty());
    assert_eq!(live_block_snapshot(&connector, &source.id), before);
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_link_to_database_retarget_blocks_before_journaled_apply() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let original_target = cleanup.create_database(
        &env.parent_page_id,
        &format!(
            "Locality live database link retarget original {}",
            unique_suffix()
        ),
    );
    let replacement_target = cleanup.create_database(
        &env.parent_page_id,
        &format!(
            "Locality live database link retarget replacement {}",
            unique_suffix()
        ),
    );
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!(
            "Locality live database link retarget source {}",
            unique_suffix()
        ),
        vec![json!({
            "object": "block",
            "type": "link_to_page",
            "link_to_page": { "type": "database_id", "database_id": original_target.id }
        })],
    );
    let connector = NotionConnector::new(live_notion_config());
    let before = live_block_snapshot(&connector, &source.id);
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let link_line = original
        .lines()
        .find(|line| {
            line.starts_with("[Linked database](")
                && line.contains(&compact_notion_id(&original_target.id))
        })
        .expect("rendered link_to_database line");
    let edited_link_line = link_line.replace(
        &compact_notion_id(&original_target.id),
        &compact_notion_id(&replacement_target.id),
    );
    assert_ne!(link_line, edited_link_line);
    fs::write(&page_path, original.replace(link_line, &edited_link_line))
        .expect("write live link_to_database retarget");

    let diff = run_diff(&store, &page_path).expect("diff live link_to_database retarget");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(
        diff.validation[0].code,
        "notion_link_to_page_retarget_unsupported"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live link_to_database retarget");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(store.list_journal().expect("journal").is_empty());
    assert_eq!(live_block_snapshot(&connector, &source.id), before);
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_notion_link_labeled_like_link_to_page_can_be_edited() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let original_target = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live paragraph link original {}", unique_suffix()),
        vec![paragraph_child("Original target for paragraph link.")],
    );
    let replacement_target = cleanup.create_page(
        &env.parent_page_id,
        &format!(
            "Locality live paragraph link replacement {}",
            unique_suffix()
        ),
        vec![paragraph_child("Replacement target for paragraph link.")],
    );
    let original_url = notion_object_url(&original_target.id);
    let replacement_url = notion_object_url(&replacement_target.id);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live paragraph link source {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": { "rich_text": [linked_text_part("Linked page", &original_url)] }
        })],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let original_line = format!("[Linked page]({original_url})");
    assert!(original.contains(&original_line), "{original}");
    fs::write(
        &page_path,
        original.replace(&original_line, &format!("[Linked page]({replacement_url})")),
    )
    .expect("write live paragraph link edit");

    let diff = run_diff(&store, &page_path).expect("diff live paragraph link edit");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("paragraph link edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live paragraph link edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after paragraph link edit");
    assert_eq!(first["block"]["type"], "paragraph");
    let first_json = serde_json::to_string(first).expect("paragraph block json");
    assert!(
        first_json.contains(&replacement_target.id)
            || first_json.contains(&compact_notion_id(&replacement_target.id)),
        "{after:#?}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_link_with_parentheses_href_can_be_edited() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let href = "https://example.com/docs/foo)";
    let markdown_href = href.replace(')', "\\)");
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live paragraph paren link {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": { "rich_text": [linked_text_part("Paren link", href)] }
        })],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let original_line = format!("[Paren link]({markdown_href})");
    assert!(original.contains(&original_line), "{original}");
    fs::write(
        &page_path,
        original.replace(
            &original_line,
            &format!("[Paren link changed]({markdown_href})"),
        ),
    )
    .expect("write live parenthesized paragraph link edit");

    let diff = run_diff(&store, &page_path).expect("diff live parenthesized paragraph link edit");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("paragraph link edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live parenthesized paragraph link edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after paragraph link edit");
    assert_eq!(first["block"]["type"], "paragraph");
    let link_url = first["block"]["paragraph"]["rich_text"][0]["text"]["link"]["url"]
        .as_str()
        .expect("live paragraph link url");
    assert_eq!(link_url, href, "{after:#?}");

    let verified = render_live_page(&connector, &source.id, &page_path);
    assert!(
        verified.contains(&format!("[Paren link changed]({markdown_href})")),
        "verified markdown should preserve the escaped parenthesized href:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_literal_break_tag_edits_preserve_literal_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live literal break tag {}", unique_suffix()),
        vec![paragraph_child("Literal <br> tag")],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    assert!(
        original.contains("Literal \\<br> tag"),
        "literal break tag should render escaped:\n{original}"
    );

    fs::write(
        &page_path,
        original.replace("Literal \\<br> tag", "Literal \\<br> tag changed"),
    )
    .expect("write live literal break tag edit");

    let diff = run_diff(&store, &page_path).expect("diff live literal break tag edit");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("literal break tag edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live literal break tag edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after literal break tag edit");
    let plain_text = first["block"]["paragraph"]["rich_text"][0]["plain_text"]
        .as_str()
        .expect("live paragraph plain text");
    assert_eq!(plain_text, "Literal <br> tag changed", "{after:#?}");

    let verified = render_live_page(&connector, &source.id, &page_path);
    assert!(
        verified.contains("Literal \\<br> tag changed"),
        "verified markdown should keep literal break tag escaped:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_literal_underline_tag_edits_preserve_literal_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live literal underline tag {}", unique_suffix()),
        vec![paragraph_child("Literal <u>tag</u>")],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    assert!(
        original.contains("Literal \\<u>tag\\</u>"),
        "literal underline tags should render escaped:\n{original}"
    );

    fs::write(
        &page_path,
        original.replace("Literal \\<u>tag\\</u>", "Literal \\<u>tag\\</u> changed"),
    )
    .expect("write live literal underline tag edit");

    let diff = run_diff(&store, &page_path).expect("diff live literal underline tag edit");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("literal underline tag edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live literal underline tag edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after literal underline tag edit");
    let plain_text = first["block"]["paragraph"]["rich_text"][0]["plain_text"]
        .as_str()
        .expect("live paragraph plain text");
    assert_eq!(plain_text, "Literal <u>tag</u> changed", "{after:#?}");
    assert!(
        first["block"]["paragraph"]["rich_text"][0]["annotations"]["underline"] != true,
        "literal underline tag must not become underline formatting: {after:#?}"
    );

    let verified = render_live_page(&connector, &source.id, &page_path);
    assert!(
        verified.contains("Literal \\<u>tag\\</u> changed"),
        "verified markdown should keep literal underline tags escaped:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_literal_equation_marker_edits_preserve_literal_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live literal equation marker {}", unique_suffix()),
        vec![paragraph_child("Literal $E=mc^2$ text")],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    assert!(
        original.contains("Literal \\$E=mc^2\\$ text"),
        "literal equation markers should render escaped:\n{original}"
    );

    fs::write(
        &page_path,
        original.replace(
            "Literal \\$E=mc^2\\$ text",
            "Literal \\$E=mc^2\\$ text changed",
        ),
    )
    .expect("write live literal equation marker edit");

    let diff = run_diff(&store, &page_path).expect("diff live literal equation marker edit");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff
        .plan
        .as_ref()
        .expect("literal equation marker edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live literal equation marker edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after literal equation marker edit");
    let rich_text = &first["block"]["paragraph"]["rich_text"][0];
    assert_eq!(rich_text["type"], "text", "{after:#?}");
    assert_eq!(
        rich_text["text"]["content"], "Literal $E=mc^2$ text changed",
        "{after:#?}"
    );
    assert!(
        rich_text["equation"].is_null(),
        "literal equation markers must not become equation rich text: {after:#?}"
    );

    let verified = render_live_page(&connector, &source.id, &page_path);
    assert!(
        verified.contains("Literal \\$E=mc^2\\$ text changed"),
        "verified markdown should keep literal equation markers escaped:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_literal_explicit_mention_marker_edits_preserve_literal_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live literal mention marker {}", unique_suffix()),
        vec![paragraph_child("Literal @date(2026-06-14) marker")],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    assert!(
        original.contains("Literal \\@date(2026-06-14) marker"),
        "literal explicit mention markers should render escaped:\n{original}"
    );

    fs::write(
        &page_path,
        original.replace(
            "Literal \\@date(2026-06-14) marker",
            "Literal \\@date(2026-06-14) marker changed",
        ),
    )
    .expect("write live literal explicit mention marker edit");

    let diff =
        run_diff(&store, &page_path).expect("diff live literal explicit mention marker edit");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff
        .plan
        .as_ref()
        .expect("literal explicit mention marker edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live literal explicit mention marker edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after literal explicit mention marker edit");
    let rich_text = &first["block"]["paragraph"]["rich_text"][0];
    assert_eq!(rich_text["type"], "text", "{after:#?}");
    assert_eq!(
        rich_text["text"]["content"], "Literal @date(2026-06-14) marker changed",
        "{after:#?}"
    );
    assert!(
        rich_text["mention"].is_null(),
        "literal explicit mention marker must not become mention rich text: {after:#?}"
    );

    let verified = render_live_page(&connector, &source.id, &page_path);
    assert!(
        verified.contains("Literal \\@date(2026-06-14) marker changed"),
        "verified markdown should keep literal explicit mention markers escaped:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_literal_markdown_inline_marker_edits_preserve_literal_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live literal markdown marker {}", unique_suffix()),
        vec![paragraph_child(
            "Literal **bold** _italic_ ~~strike~~ `code` [link](https://example.com)",
        )],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    assert!(
        original.contains(
            "Literal \\**bold\\** \\_italic\\_ \\~~strike\\~~ \\`code\\` \\[link](https://example.com)"
        ),
        "literal markdown inline markers should render escaped:\n{original}"
    );

    fs::write(
        &page_path,
        original.replace(
            "Literal \\**bold\\** \\_italic\\_ \\~~strike\\~~ \\`code\\` \\[link](https://example.com)",
            "Literal \\**bold\\** \\_italic\\_ \\~~strike\\~~ \\`code\\` \\[link](https://example.com) changed",
        ),
    )
    .expect("write live literal markdown inline marker edit");

    let diff = run_diff(&store, &page_path).expect("diff live literal markdown inline marker edit");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff
        .plan
        .as_ref()
        .expect("literal markdown inline marker edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live literal markdown inline marker edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after literal markdown inline marker edit");
    let rich_text = first["block"]["paragraph"]["rich_text"]
        .as_array()
        .expect("live paragraph rich text");
    let plain_text = rich_text
        .iter()
        .map(|part| part["plain_text"].as_str().unwrap_or_default())
        .collect::<String>();
    assert_eq!(
        plain_text,
        "Literal **bold** _italic_ ~~strike~~ `code` [link](https://example.com) changed",
        "{after:#?}"
    );
    for part in rich_text {
        assert_eq!(part["type"], "text", "{after:#?}");
        assert!(part["text"]["link"].is_null(), "{after:#?}");
        assert!(part["mention"].is_null(), "{after:#?}");
        assert!(part["equation"].is_null(), "{after:#?}");
        assert!(part["annotations"]["bold"] != true, "{after:#?}");
        assert!(part["annotations"]["italic"] != true, "{after:#?}");
        assert!(part["annotations"]["strikethrough"] != true, "{after:#?}");
        assert!(part["annotations"]["code"] != true, "{after:#?}");
    }

    let verified = render_live_page(&connector, &source.id, &page_path);
    assert!(
        verified.contains(
            "Literal \\**bold\\** \\_italic\\_ \\~~strike\\~~ \\`code\\` \\[link](https://example.com) changed"
        ),
        "verified markdown should keep literal markdown inline markers escaped:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_literal_block_marker_edits_preserve_paragraph_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live literal block marker {}", unique_suffix()),
        vec![paragraph_child("# Literal heading marker")],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    assert!(
        original.contains("\\# Literal heading marker"),
        "literal block marker should render escaped:\n{original}"
    );

    fs::write(
        &page_path,
        original.replace(
            "\\# Literal heading marker",
            "\\# Literal heading marker changed",
        ),
    )
    .expect("write live literal block marker edit");

    let diff = run_diff(&store, &page_path).expect("diff live literal block marker edit");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("literal block marker edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");
    assert_eq!(plan.summary.blocks_replaced, 0, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live literal block marker edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after literal block marker edit");
    assert_eq!(first["block"]["type"], "paragraph", "{after:#?}");
    let rich_text = &first["block"]["paragraph"]["rich_text"][0];
    assert_eq!(rich_text["type"], "text", "{after:#?}");
    assert_eq!(
        rich_text["text"]["content"], "# Literal heading marker changed",
        "{after:#?}"
    );

    let verified = render_live_page(&connector, &source.id, &page_path);
    assert!(
        verified.contains("\\# Literal heading marker changed"),
        "verified markdown should keep literal block marker escaped:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_table_width_change_blocks_before_journaled_apply() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live table width guard {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "table",
            "table": {
                "table_width": 2,
                "has_column_header": true,
                "has_row_header": false,
                "children": [
                    table_row_child("Task", "Owner"),
                    table_row_child("Seed", "Alex")
                ]
            }
        })],
    );
    let connector = NotionConnector::new(live_notion_config());
    let before = live_block_snapshot(&connector, &source.id);
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let edited = original
        .replace("| Task | Owner |", "| Task | Owner | Status |")
        .replace("| --- | --- |", "| --- | --- | --- |")
        .replace("| Seed | Alex |", "| Seed | Alex | Todo |");
    assert_ne!(edited, original, "{original}");
    fs::write(&page_path, edited).expect("write live table width edit");

    let diff = run_diff(&store, &page_path).expect("diff live table width edit");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(
        diff.validation[0].code,
        "notion_table_width_change_unsupported"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live table width edit");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(store.list_journal().expect("journal").is_empty());
    assert_eq!(live_block_snapshot(&connector, &source.id), before);
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_table_middle_row_delete_blocks_before_journaled_apply() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live table middle row guard {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "table",
            "table": {
                "table_width": 2,
                "has_column_header": true,
                "has_row_header": false,
                "children": [
                    table_row_child("Name", "Status"),
                    table_row_child("Alpha", "Todo"),
                    table_row_child("Beta", "Doing"),
                    table_row_child("Gamma", "Done")
                ]
            }
        })],
    );
    let connector = NotionConnector::new(live_notion_config());
    let before = live_block_snapshot(&connector, &source.id);
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let beta_row = "\n| Beta | Doing |";
    assert!(original.contains(beta_row), "{original}");
    fs::write(&page_path, original.replace(beta_row, "")).expect("delete live table middle row");

    let diff = run_diff(&store, &page_path).expect("diff live table middle row delete");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(
        diff.validation[0].code,
        "notion_table_middle_row_delete_unsupported"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live table middle row delete");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(store.list_journal().expect("journal").is_empty());
    assert_eq!(live_block_snapshot(&connector, &source.id), before);
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_table_trailing_row_delete_pushes_and_reconciles() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!(
            "Locality live table trailing row delete {}",
            unique_suffix()
        ),
        vec![json!({
            "object": "block",
            "type": "table",
            "table": {
                "table_width": 2,
                "has_column_header": true,
                "has_row_header": false,
                "children": [
                    table_row_child("Name", "Status"),
                    table_row_child("Keep task", "Todo"),
                    table_row_child("Drop task", "Later")
                ]
            }
        })],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    assert!(original.contains("| Drop task | Later |"), "{original}");
    let trailing_newline = if original.ends_with('\n') { "\n" } else { "" };
    let edited = original
        .lines()
        .filter(|line| *line != "| Drop task | Later |")
        .collect::<Vec<_>>()
        .join("\n")
        + trailing_newline;
    assert_ne!(edited, original, "{original}");
    fs::write(&page_path, edited).expect("delete live table trailing row");

    let diff = run_diff(&store, &page_path).expect("diff live table trailing row delete");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("table trailing row delete plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live table trailing row delete");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.apply_effect_count, 1);

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean table trailing row delete status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let verified = render_live_page(&connector, &source.id, &page_path);
    assert!(verified.contains("| Keep task | Todo |"), "{verified}");
    assert!(
        !verified.contains("| Drop task | Later |"),
        "trailing table row should be archived remotely and removed locally:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_child_page_link_move_blocks_before_journaled_apply() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let anchor_text = "Anchor before child page.";
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live child link move parent {}", unique_suffix()),
        vec![paragraph_child(anchor_text)],
    );
    let child_title = format!("Locality live child link move child {}", unique_suffix());
    let child = cleanup.create_page(
        &parent.id,
        &child_title,
        vec![paragraph_child("Child page body.")],
    );
    let connector = NotionConnector::new(live_notion_config());
    let before = live_block_snapshot(&connector, &parent.id);
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &parent.id);
    let child_line = original
        .lines()
        .find(|line| line.contains(&child_title) && line.contains(&compact_notion_id(&child.id)))
        .expect("rendered child_page line");
    let original_order = format!("{anchor_text}\n\n{child_line}\n");
    assert!(original.contains(&original_order), "{original}");
    fs::write(
        &page_path,
        original.replace(
            &original_order,
            &format!("{child_line}\n\n{anchor_text}\n\n"),
        ),
    )
    .expect("write live child_page link move");

    let diff = run_diff(&store, &page_path).expect("diff live child_page link move");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(
        diff.validation[0].code,
        "notion_child_page_link_move_unsupported"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live child_page link move");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(store.list_journal().expect("journal").is_empty());
    assert_eq!(live_block_snapshot(&connector, &parent.id), before);
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_child_page_link_delete_blocks_before_journaled_apply() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let anchor_text = "Anchor before child page delete.";
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live child link delete parent {}", unique_suffix()),
        vec![paragraph_child(anchor_text)],
    );
    let child_title = format!("Locality live child link delete child {}", unique_suffix());
    let child = cleanup.create_page(
        &parent.id,
        &child_title,
        vec![paragraph_child("Child page body.")],
    );
    let connector = NotionConnector::new(live_notion_config());
    let before = live_block_snapshot(&connector, &parent.id);
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &parent.id);
    let child_line = original
        .lines()
        .find(|line| line.contains(&child_title) && line.contains(&compact_notion_id(&child.id)))
        .expect("rendered child_page line");
    let line_to_delete = format!("\n\n{child_line}\n");
    assert!(original.contains(&line_to_delete), "{original}");
    fs::write(&page_path, original.replace(&line_to_delete, "\n")).expect("delete child link");

    let diff = run_diff(&store, &page_path).expect("diff live child_page link delete");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(
        diff.validation[0].code,
        "notion_child_page_link_delete_unsupported"
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live child_page link delete");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(store.list_journal().expect("journal").is_empty());
    assert_eq!(live_block_snapshot(&connector, &parent.id), before);

    let child_page = cleanup
        .api
        .retrieve_page(&child.id)
        .expect("retrieve child after blocked delete");
    assert!(
        !child_page.archived && !child_page.in_trash,
        "child page should not be archived by blocked parent-link delete: {child_page:#?}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_lazy_virtual_mount_enumerates_children_and_hydrates_on_open() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live lazy root {}", unique_suffix()),
        vec![paragraph_child(
            "Root page body should not materialize during directory listing.",
        )],
    );
    let child = cleanup.create_page(
        &scratch.id,
        &format!("Locality live lazy child {}", unique_suffix()),
        vec![paragraph_child(
            "Lazy child body materialized only on open.",
        )],
    );
    let connector = NotionConnector::new(
        live_notion_config().with_root_page_id(RemoteId::new(scratch.id.clone())),
    );
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    mount_virtual_workspace(&fixture, &mut store, &scratch.id);
    let content_root = fixture.content_root();

    let root_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        ROOT_CONTAINER_IDENTIFIER,
    )
    .expect("list virtual root");
    assert!(
        root_children
            .children
            .iter()
            .any(|item| item.filename == fixture.virtual_root_directory_name()),
        "{root_children:#?}"
    );

    let mount_point_root = fixture.virtual_root_identifier();
    let refreshed =
        refresh_virtual_fs_children(&mut store, &connector, &fixture.mount_id, &mount_point_root)
            .expect("refresh mount point root metadata");
    assert_eq!(refreshed.saved, 1);
    assert!(refreshed.changed);
    let mount_point_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &mount_point_root,
    )
    .expect("list mount point root");
    let scratch_folder = find_virtual_folder(&mount_point_children.children, &scratch.id);
    assert!(
        !content_root
            .join(&scratch_folder.path)
            .join("page.md")
            .exists(),
        "listing the mount point root must not hydrate the root page body"
    );

    let refreshed = refresh_virtual_fs_children(
        &mut store,
        &connector,
        &fixture.mount_id,
        &scratch_folder.identifier,
    )
    .expect("refresh page children metadata");
    assert_eq!(refreshed.saved, 1);
    assert!(refreshed.changed);
    let nested_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &scratch_folder.identifier,
    )
    .expect("list nested page children");
    let child_folder = find_virtual_folder(&nested_children.children, &child.id);
    assert!(
        !content_root
            .join(&child_folder.path)
            .join("page.md")
            .exists(),
        "listing a page directory must not hydrate nested page bodies"
    );

    let materialized = materialize_virtual_fs_item_with_content_root(
        &mut store,
        &connector,
        &content_root,
        &fixture.mount_id,
        &child.id,
    )
    .expect("open child page");
    assert_eq!(materialized.hydration, HydrationState::Hydrated);
    let materialized_path = PathBuf::from(materialized.path);
    let markdown = fs::read_to_string(&materialized_path).expect("read hydrated virtual file");
    assert!(markdown.contains("Lazy child body materialized only on open."));
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_drift_preflight_blocks_push_before_overwriting_remote() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live drift {}", unique_suffix()),
        vec![paragraph_child("Base body before local and remote drift.")],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);
    let local_marker = format!("Local pending edit {}", unique_suffix());
    let remote_marker = format!("Remote competing edit {}", unique_suffix());
    fs::write(
        &page_path,
        original.replace("Base body before local and remote drift.", &local_marker),
    )
    .expect("write local drift edit");

    append_remote_paragraph_and_wait(
        &cleanup.api,
        &scratch.id,
        &remote_marker,
        "drift preflight remote edit",
    );

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push drifted page");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "apply_failed", "{push:#?}");
    let drift_message = format!(
        "{} {}",
        push.message.as_deref().unwrap_or_default(),
        push.suggested_fix.as_deref().unwrap_or_default()
    );
    assert!(drift_message.contains("changed since"), "{push:#?}");

    let verified = render_live_page(&connector, &scratch.id, &page_path);
    assert!(verified.contains(&remote_marker), "{verified}");
    assert!(
        !verified.contains(&local_marker),
        "remote content should not be overwritten by a blocked push:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_dirty_pull_conflict_can_be_resolved_and_pushed() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let base = "Conflict base body before local and remote edits.";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live conflict resolve {}", unique_suffix()),
        vec![paragraph_child(base)],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);
    let local_marker = format!("Local conflict edit {}", unique_suffix());
    let remote_marker = format!("Remote conflict edit {}", unique_suffix());
    let resolved_marker = format!("Resolved conflict edit {}", unique_suffix());

    fs::write(&page_path, original.replace(base, &local_marker))
        .expect("write local conflict edit");

    let paragraph_id = first_live_child_block_id(&cleanup.api, &scratch.id);
    update_remote_paragraph_and_wait(
        &cleanup.api,
        &scratch.id,
        &paragraph_id,
        &remote_marker,
        "dirty pull conflict remote edit",
    );

    let pull = run_pull(&mut store, &connector, &page_path).expect("pull conflicted live page");
    assert!(!pull.ok, "{pull:#?}");
    assert_eq!(pull.hydrated, 0, "{pull:#?}");
    assert_eq!(pull.skipped_dirty, 1, "{pull:#?}");
    assert_eq!(pull.conflicts.len(), 1, "{pull:#?}");

    let conflicted = fs::read_to_string(&page_path).expect("read conflicted live page");
    assert!(conflicted.contains(&local_marker), "{conflicted}");
    assert!(conflicted.contains(&remote_marker), "{conflicted}");
    assert!(conflicted.contains(CONFLICT_LOCAL_MARKER), "{conflicted}");
    assert!(
        conflicted.contains(CONFLICT_SEPARATOR_MARKER),
        "{conflicted}"
    );
    assert!(conflicted.contains(CONFLICT_REMOTE_MARKER), "{conflicted}");
    assert!(has_unresolved_conflict_markers(&conflicted));
    let conflicted_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("conflicted status");
    assert_eq!(
        conflicted_status.summary.conflicted, 1,
        "{conflicted_status:#?}"
    );

    fs::write(&page_path, original.replace(base, &resolved_marker))
        .expect("write resolved conflict");
    let diff = run_diff(&store, &page_path).expect("diff resolved conflict");
    assert!(diff.validation.is_empty(), "{diff:#?}");
    let plan = diff.plan.as_ref().expect("resolved conflict plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push resolved conflict");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let verified = render_live_page(&connector, &scratch.id, &page_path);
    assert!(verified.contains(&resolved_marker), "{verified}");
    assert!(!verified.contains(&local_marker), "{verified}");
    assert!(!verified.contains(&remote_marker), "{verified}");
    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path),
            ..StatusOptions::default()
        },
    )
    .expect("clean status after conflict resolution push");
    assert!(clean_status.clean, "{clean_status:#?}");
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_macos_file_provider_dirty_pull_conflict_materializes_visible_markers() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let base = "File Provider conflict base body before local and remote edits.";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live macOS conflict {}", unique_suffix()),
        vec![paragraph_child(base)],
    );
    let connector = NotionConnector::new(live_notion_config());
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(scratch.id.clone())),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::MacosFileProvider,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live macos file provider page");
    run_pull_with_state_root(
        &mut store,
        &connector,
        &fixture.root,
        Some(&fixture.state_root),
    )
    .expect("pull live page into daemon cache");

    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new(scratch.id.clone()))
        .expect("get live scratch entity")
        .expect("live scratch entity");
    let cache_path = fixture.content_root().join(&entity.path);
    let visible_path = fixture.root.join(&entity.path);
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::copy(&cache_path, &visible_path).expect("seed visible File Provider replica");
    let original = fs::read_to_string(&visible_path).expect("read visible replica");
    let local_marker = format!("Local visible conflict edit {}", unique_suffix());
    let remote_marker = format!("Remote conflict edit {}", unique_suffix());
    fs::write(&visible_path, original.replace(base, &local_marker))
        .expect("write missed visible local edit");

    let paragraph_id = first_live_child_block_id(&cleanup.api, &scratch.id);
    update_remote_paragraph_and_wait(
        &cleanup.api,
        &scratch.id,
        &paragraph_id,
        &remote_marker,
        "macOS File Provider conflict remote edit",
    );

    let pull = run_pull_with_state_root(
        &mut store,
        &connector,
        &visible_path,
        Some(&fixture.state_root),
    )
    .expect("pull conflicted visible page");
    assert!(!pull.ok, "{pull:#?}");
    assert_eq!(pull.hydrated, 0, "{pull:#?}");
    assert_eq!(pull.skipped_dirty, 1, "{pull:#?}");
    assert_eq!(pull.conflicts.len(), 1, "{pull:#?}");

    let visible = fs::read_to_string(&visible_path).expect("read visible conflicted page");
    assert!(visible.contains(&local_marker), "{visible}");
    assert!(visible.contains(&remote_marker), "{visible}");
    assert!(visible.contains(CONFLICT_LOCAL_MARKER), "{visible}");
    assert!(visible.contains(CONFLICT_SEPARATOR_MARKER), "{visible}");
    assert!(visible.contains(CONFLICT_REMOTE_MARKER), "{visible}");
    assert!(has_unresolved_conflict_markers(&visible), "{visible}");
    let cached = fs::read_to_string(&cache_path).expect("read daemon conflict cache");
    assert_eq!(visible, cached);
    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new(scratch.id))
        .expect("get conflicted entity")
        .expect("conflicted entity");
    assert_eq!(entity.hydration, HydrationState::Conflicted);
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_inspect_explains_remote_and_local_drift_without_mutating() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let base = "Inspect base body before drift.";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live inspect {}", unique_suffix()),
        vec![paragraph_child(base)],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, store, page_path, original) = pull_live_page(&connector, &scratch.id);

    let initial = run_inspect(
        &store,
        &connector,
        InspectOptions {
            path: page_path.clone(),
            state_root: None,
        },
    )
    .expect("initial live inspect");
    assert!(initial.ok, "{initial:#?}");
    assert_eq!(initial.explanation.state, RemoteChangeState::AllSynced);
    assert_eq!(initial.explanation.action, RemoteChangeAction::None);

    let remote_marker = format!("Inspect remote drift {}", unique_suffix());
    append_remote_paragraph(&cleanup.api, &scratch.id, &remote_marker);
    let remote_only = run_inspect(
        &store,
        &connector,
        InspectOptions {
            path: page_path.clone(),
            state_root: None,
        },
    )
    .expect("remote drift live inspect");
    assert!(remote_only.ok, "{remote_only:#?}");
    assert_eq!(
        remote_only.explanation.state,
        RemoteChangeState::RemoteChangedOnly
    );
    assert_eq!(
        remote_only.explanation.action,
        RemoteChangeAction::SafeToFastForward
    );
    assert!(!remote_only.explanation.local.changed);
    assert!(remote_only.explanation.remote.changed);
    assert_eq!(
        fs::read_to_string(&page_path).expect("read after remote-only inspect"),
        original,
        "inspect must not fast-forward or otherwise mutate local content"
    );

    let local_marker = format!("Inspect local drift {}", unique_suffix());
    fs::write(&page_path, original.replace(base, &local_marker))
        .expect("write inspect local drift");
    let both_changed = run_inspect(
        &store,
        &connector,
        InspectOptions {
            path: page_path.clone(),
            state_root: None,
        },
    )
    .expect("both changed live inspect");
    assert!(both_changed.ok, "{both_changed:#?}");
    assert_eq!(
        both_changed.explanation.state,
        RemoteChangeState::BothChanged
    );
    assert_eq!(
        both_changed.explanation.action,
        RemoteChangeAction::ReviewBeforePush
    );
    assert!(both_changed.explanation.local.changed);
    assert!(both_changed.explanation.remote.changed);
    let after_both = fs::read_to_string(&page_path).expect("read after both-changed inspect");
    assert!(after_both.contains(&local_marker), "{after_both}");
    assert!(
        !after_both.contains(&remote_marker),
        "inspect must not write remote drift into the local file:\n{after_both}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_push_log_and_undo_restores_remote_content() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let base = "Undo base body before push.";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live undo {}", unique_suffix()),
        vec![paragraph_child(base)],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);
    let pushed_marker = format!("Undo pushed edit {}", unique_suffix());

    fs::write(&page_path, original.replace(base, &pushed_marker)).expect("write undo edit");
    let diff = run_diff(&store, &page_path).expect("diff undo edit");
    let plan = diff.plan.as_ref().expect("undo edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push undo edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.journal_status.as_deref(), Some("reconciled"));
    let push_id = push.push_id.clone().expect("push id");
    let pushed_remote = render_live_page(&connector, &scratch.id, &page_path);
    assert!(pushed_remote.contains(&pushed_marker), "{pushed_remote}");

    let log = run_log(
        &store,
        LogOptions {
            path: Some(page_path.clone()),
            ..LogOptions::default()
        },
    )
    .expect("log undo push");
    assert_eq!(log.entries.len(), 1, "{log:#?}");
    assert_eq!(log.entries[0].push_id, push_id);
    assert_eq!(log.entries[0].status, "reconciled");
    assert_eq!(log.entries[0].preimage_count, 1);
    assert_eq!(log.entries[0].apply_effect_count, 1);

    let mut undo_applier = ConnectorUndoApplier::new(&connector);
    let undo = run_undo_with_applier(&mut store, push_id.clone(), &mut undo_applier)
        .expect("undo live push");
    assert!(undo.ok, "{undo:#?}");
    assert_eq!(undo.action, "reverse_applied", "{undo:#?}");
    assert_eq!(undo.status, "reverted");
    assert_eq!(undo.changed_remote_ids, vec![scratch.id.clone()]);

    let restored_remote = render_live_page(&connector, &scratch.id, &page_path);
    assert!(restored_remote.contains(base), "{restored_remote}");
    assert!(
        !restored_remote.contains(&pushed_marker),
        "undo should restore remote content:\n{restored_remote}"
    );
    let reverted_log = run_log(
        &store,
        LogOptions {
            path: Some(page_path),
            ..LogOptions::default()
        },
    )
    .expect("log reverted undo push");
    assert_eq!(reverted_log.entries[0].push_id, push_id);
    assert_eq!(reverted_log.entries[0].status, "reverted");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_undo_archive_restores_paragraph_link_without_link_to_page_promotion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let target = cleanup.create_page(
        &env.parent_page_id,
        &format!(
            "Locality live undo paragraph link target {}",
            unique_suffix()
        ),
        vec![paragraph_child("Target for paragraph-link undo.")],
    );
    let target_url = notion_object_url(&target.id);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!(
            "Locality live undo paragraph link source {}",
            unique_suffix()
        ),
        vec![json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": { "rich_text": [linked_text_part("Linked page", &target_url)] }
        })],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);
    let link_line = format!("[Linked page]({target_url})");
    assert!(original.contains(&link_line), "{original}");
    fs::write(&page_path, original.replace(&format!("{link_line}\n"), ""))
        .expect("archive paragraph link locally");

    let diff = run_diff(&store, &page_path).expect("diff paragraph link archive");
    let plan = diff.plan.as_ref().expect("paragraph link archive plan");
    assert_eq!(plan.summary.blocks_archived, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push paragraph link archive");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    let push_id = push.push_id.clone().expect("push id");

    let archived = render_live_page(&connector, &scratch.id, &page_path);
    assert!(!archived.contains(&link_line), "{archived}");

    let mut undo_applier = ConnectorUndoApplier::new(&connector);
    let undo = run_undo_with_applier(&mut store, push_id.clone(), &mut undo_applier)
        .expect("undo paragraph link archive");
    assert!(undo.ok, "{undo:#?}");
    assert_eq!(undo.action, "reverse_applied", "{undo:#?}");
    assert_eq!(undo.status, "reverted");

    let after = live_block_snapshot(&connector, &scratch.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("restored paragraph-link block");
    assert_eq!(first["block"]["type"], "paragraph", "{after:#?}");
    let first_json = serde_json::to_string(first).expect("paragraph block json");
    assert!(
        first_json.contains(&target.id) || first_json.contains(&compact_notion_id(&target.id)),
        "{after:#?}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_page_directory_create_pushes_child_page_and_refreshes_parent() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live page-dir parent {}", unique_suffix()),
        vec![paragraph_child("Parent body before child page creation.")],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, parent_page_path, _markdown) =
        pull_live_page(&connector, &scratch.id);
    let child_title = format!("Locality live page-dir child {}", unique_suffix());
    let child_dir = parent_page_path
        .parent()
        .expect("parent page directory")
        .join(slug_for_test(&child_title));
    fs::create_dir_all(&child_dir).expect("create child page directory");
    let child_page_path = child_dir.join("page.md");
    fs::write(
        &child_page_path,
        format!("---\ntitle: \"{child_title}\"\n---\n# Created child\n\nCreated from page.md.\n"),
    )
    .expect("write child page.md");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &child_page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push child page create");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    let created_page_id = push
        .changed_remote_ids
        .iter()
        .find(|id| *id != &scratch.id)
        .expect("created page id")
        .clone();
    cleanup.block_ids.push(created_page_id.clone());

    let child_markdown = render_live_markdown(&connector, &created_page_id, &child_page_path);
    assert!(child_markdown.contains(&format!("title: \"{child_title}\"")));
    assert!(child_markdown.contains("Created from page.md."));

    let parent_markdown = fs::read_to_string(&parent_page_path).expect("read reconciled parent");
    assert!(
        parent_markdown.contains(&child_title),
        "parent page should reconcile the Notion child_page link:\n{parent_markdown}"
    );
    let remote_parent = render_live_markdown(&connector, &scratch.id, &parent_page_path);
    assert!(
        remote_parent.contains(&child_title),
        "remote parent should render the new child page link:\n{remote_parent}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_undo_child_page_create_archives_remote_page() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live undo page create parent {}", unique_suffix()),
        vec![paragraph_child(
            "Parent body before undoing child creation.",
        )],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (_fixture, mut store, parent_page_path, _markdown) =
        pull_live_page(&connector, &scratch.id);
    let child_title = format!("Locality live undo page create child {}", unique_suffix());
    let child_dir = parent_page_path
        .parent()
        .expect("parent page directory")
        .join(slug_for_test(&child_title));
    fs::create_dir_all(&child_dir).expect("create undo child page directory");
    let child_page_path = child_dir.join("page.md");
    fs::write(
        &child_page_path,
        format!("---\ntitle: \"{child_title}\"\n---\nCreated child before undo.\n"),
    )
    .expect("write undo child page.md");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &child_page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push child page create before undo");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.journal_status.as_deref(), Some("reconciled"));
    let push_id = push.push_id.clone().expect("push id");
    let created_page_id = push
        .changed_remote_ids
        .iter()
        .find(|id| *id != &scratch.id)
        .expect("created page id")
        .clone();
    cleanup.block_ids.push(created_page_id.clone());
    let created_page = cleanup
        .api
        .retrieve_page(&created_page_id)
        .expect("retrieve created child page");
    assert!(
        !created_page.archived && !created_page.in_trash,
        "created child page should exist before undo: {created_page:#?}"
    );

    let log = run_log(
        &store,
        LogOptions {
            path: Some(child_page_path.clone()),
            ..LogOptions::default()
        },
    )
    .expect("log child page create push");
    assert_eq!(log.entries.len(), 1, "{log:#?}");
    assert_eq!(log.entries[0].push_id, push_id);
    assert_eq!(log.entries[0].status, "reconciled");
    assert_eq!(log.entries[0].apply_effect_count, 1);

    let mut undo_applier = ConnectorUndoApplier::new(&connector);
    let undo = run_undo_with_applier(&mut store, push_id.clone(), &mut undo_applier)
        .expect("undo child page create");
    assert!(undo.ok, "{undo:#?}");
    assert_eq!(undo.action, "reverse_applied", "{undo:#?}");
    assert_eq!(undo.status, "reverted");

    let archived = cleanup
        .api
        .retrieve_page(&created_page_id)
        .expect("retrieve archived created child page");
    assert!(
        archived.archived || archived.in_trash,
        "undo should archive the created child page: {archived:#?}"
    );
    let reverted_log = run_log(
        &store,
        LogOptions {
            path: Some(child_page_path),
            ..LogOptions::default()
        },
    )
    .expect("log reverted child page create push");
    assert_eq!(reverted_log.entries[0].push_id, push_id);
    assert_eq!(reverted_log.entries[0].status, "reverted");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_undo_database_row_create_archives_remote_row() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live undo row create scratch {}", unique_suffix()),
        Vec::new(),
    );
    let database = cleanup.create_database(
        &scratch.id,
        &format!("Locality live undo row create database {}", unique_suffix()),
    );

    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let connector = NotionConnector::new(live_notion_config());
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(scratch.id.clone())),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live undo database row root page");
    run_pull(&mut store, &connector, &fixture.root).expect("pull live undo database row root");

    let database_dir = fixture.database_dir();
    let row_title = format!("Locality live undo created row {}", unique_suffix());
    let new_row_dir = database_dir.join(slug_for_test(&row_title));
    fs::create_dir_all(&new_row_dir).expect("create undo row directory");
    let new_row_path = new_row_dir.join("page.md");
    fs::write(
        &new_row_path,
        format!(
            "---\ntitle: \"{row_title}\"\nNotes: \"Created row before undo\"\nPoints: 34\nStatus: Todo\nDone: false\n---\n# Undo row body\n\nCreated database row before undo.\n"
        ),
    )
    .expect("write live undo database row page");

    let diff = run_diff(&store, &new_row_path).expect("diff undo database row create");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("undo row create plan");
    assert_eq!(plan.summary.entities_created, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &new_row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push database row create before undo");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.journal_status.as_deref(), Some("reconciled"));
    let push_id = push.push_id.clone().expect("push id");
    let created_row_id = push
        .changed_remote_ids
        .iter()
        .find(|id| *id != &database.id)
        .expect("created database row id")
        .clone();
    cleanup.block_ids.push(created_row_id.clone());
    let created_row = cleanup
        .api
        .retrieve_page(&created_row_id)
        .expect("retrieve created database row");
    assert!(
        !created_row.archived && !created_row.in_trash,
        "created database row should exist before undo: {created_row:#?}"
    );

    let log = run_log(
        &store,
        LogOptions {
            path: Some(new_row_path.clone()),
            ..LogOptions::default()
        },
    )
    .expect("log database row create push");
    assert_eq!(log.entries.len(), 1, "{log:#?}");
    assert_eq!(log.entries[0].push_id, push_id);
    assert_eq!(log.entries[0].status, "reconciled");
    assert_eq!(log.entries[0].apply_effect_count, 1);

    let mut undo_applier = ConnectorUndoApplier::new(&connector);
    let undo = run_undo_with_applier(&mut store, push_id.clone(), &mut undo_applier)
        .expect("undo database row create");
    assert!(undo.ok, "{undo:#?}");
    assert_eq!(undo.action, "reverse_applied", "{undo:#?}");
    assert_eq!(undo.status, "reverted");
    let undo_plan = undo.undo_plan.as_ref().expect("undo plan");
    assert!(
        undo_plan.operations.iter().any(|operation| matches!(
            operation,
            UndoOperationOutput::ArchiveCreatedEntity { entity_id }
                if entity_id == &created_row_id
        )),
        "{undo:#?}"
    );

    let archived = cleanup
        .api
        .retrieve_page(&created_row_id)
        .expect("retrieve archived created database row");
    assert!(
        archived.archived || archived.in_trash,
        "undo should archive the created database row: {archived:#?}"
    );
    let reverted_log = run_log(
        &store,
        LogOptions {
            path: Some(new_row_path),
            ..LogOptions::default()
        },
    )
    .expect("log reverted database row create push");
    assert_eq!(reverted_log.entries[0].push_id, push_id);
    assert_eq!(reverted_log.entries[0].status, "reverted");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_virtual_page_directory_delete_archives_remote_child_page() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live virtual delete parent {}", unique_suffix()),
        vec![paragraph_child("Parent body before child page delete.")],
    );
    let child_title = format!("Locality live virtual delete child {}", unique_suffix());
    let child = cleanup.create_page(
        &parent.id,
        &child_title,
        vec![paragraph_child("Child body before delete.")],
    );
    let connector = NotionConnector::new(
        live_notion_config().with_root_page_id(RemoteId::new(parent.id.clone())),
    );
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    mount_virtual_workspace(&fixture, &mut store, &parent.id);
    let content_root = fixture.content_root();
    let mount_point_root = fixture.virtual_root_identifier();
    refresh_virtual_fs_children(&mut store, &connector, &fixture.mount_id, &mount_point_root)
        .expect("refresh mount point root");
    let mount_point_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &mount_point_root,
    )
    .expect("list mount point root");
    let parent_folder = find_virtual_folder(&mount_point_children.children, &parent.id);
    refresh_virtual_fs_children(
        &mut store,
        &connector,
        &fixture.mount_id,
        &parent_folder.identifier,
    )
    .expect("refresh parent children");
    let parent_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &parent_folder.identifier,
    )
    .expect("list parent children");
    let child_folder = find_virtual_folder(&parent_children.children, &child.id);
    materialize_virtual_fs_item_with_content_root(
        &mut store,
        &connector,
        &content_root,
        &fixture.mount_id,
        &child.id,
    )
    .expect("hydrate child before delete");

    let deleted = trash_virtual_fs_item(
        &mut store,
        &content_root,
        &fixture.mount_id,
        &child_folder.identifier,
    )
    .expect("record virtual child page delete");
    assert_eq!(deleted.identifier, child_folder.identifier);
    let pending_status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("pending delete status");
    let delete_entry = pending_status
        .mounts
        .iter()
        .flat_map(|mount| mount.entries.iter())
        .find(|entry| entry.entity_id == child.id)
        .expect("pending delete status entry");
    assert_eq!(delete_entry.state.as_str(), "dirty");
    assert_eq!(delete_entry.sync_state.as_str(), "pending_local_changes");
    assert_eq!(delete_entry.issues[0].code, "pending_virtual_delete");

    let diff = run_diff(&store, &fixture.root).expect("diff virtual delete");
    let plan = diff.plan.as_ref().expect("virtual delete plan");
    assert_eq!(diff.action, "confirm_plan");
    assert_eq!(plan.summary.entities_archived, 1, "{plan:#?}");
    assert_eq!(plan.affected_entities, vec![child.id.clone()]);

    let push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &fixture.root,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push virtual child page delete");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.journal_status.as_deref(), Some("reconciled"));
    assert_eq!(push.changed_remote_ids, vec![child.id.clone()]);
    assert_eq!(push.reconciled_remote_ids, vec![child.id.clone()]);
    assert_eq!(push.apply_effect_count, 1);

    let archived = cleanup
        .api
        .retrieve_page(&child.id)
        .expect("retrieve archived child page");
    assert!(
        archived.archived || archived.in_trash,
        "child page should be archived after virtual delete: {archived:#?}"
    );
    assert!(
        store
            .get_entity(&fixture.mount_id, &RemoteId::new(child.id.clone()))
            .expect("get deleted child entity")
            .is_none(),
        "reconcile should remove archived child entity from local state"
    );
    assert!(
        store
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list mutations after delete push")
            .is_empty(),
        "reconcile should clear the pending delete mutation"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_virtual_database_row_directory_delete_archives_remote_row() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!(
            "Locality live virtual row delete parent {}",
            unique_suffix()
        ),
        Vec::new(),
    );
    let database = cleanup.create_database(
        &parent.id,
        &format!(
            "Locality live virtual row delete database {}",
            unique_suffix()
        ),
    );
    let row = cleanup.create_database_row(
        &database,
        &format!("Locality live virtual row delete row {}", unique_suffix()),
        serde_json::Map::new(),
        vec![paragraph_child("Row body before virtual delete.")],
    );
    let connector = NotionConnector::new(
        live_notion_config().with_root_page_id(RemoteId::new(parent.id.clone())),
    );
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    mount_virtual_workspace(&fixture, &mut store, &parent.id);
    let content_root = fixture.content_root();
    let mount_point_root = fixture.virtual_root_identifier();
    refresh_virtual_fs_children(&mut store, &connector, &fixture.mount_id, &mount_point_root)
        .expect("refresh mount point root");
    let mount_point_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &mount_point_root,
    )
    .expect("list mount point root");
    let parent_folder = find_virtual_folder(&mount_point_children.children, &parent.id);
    refresh_virtual_fs_children(
        &mut store,
        &connector,
        &fixture.mount_id,
        &parent_folder.identifier,
    )
    .expect("refresh parent children");
    let parent_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &parent_folder.identifier,
    )
    .expect("list parent children");
    let database_folder = find_virtual_folder(&parent_children.children, &database.id);
    refresh_virtual_fs_children(
        &mut store,
        &connector,
        &fixture.mount_id,
        &database_folder.identifier,
    )
    .expect("refresh database row children");
    let database_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &database_folder.identifier,
    )
    .expect("list database row children");
    let row_folder = find_virtual_folder(&database_children.children, &row.id);
    materialize_virtual_fs_item_with_content_root(
        &mut store,
        &connector,
        &content_root,
        &fixture.mount_id,
        &row.id,
    )
    .expect("hydrate database row before delete");

    let deleted = trash_virtual_fs_item(
        &mut store,
        &content_root,
        &fixture.mount_id,
        &row_folder.identifier,
    )
    .expect("record virtual database row delete");
    assert_eq!(deleted.identifier, row_folder.identifier);
    let pending_status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("pending database row delete status");
    let delete_entry = pending_status
        .mounts
        .iter()
        .flat_map(|mount| mount.entries.iter())
        .find(|entry| entry.entity_id == row.id)
        .expect("pending database row delete status entry");
    assert_eq!(delete_entry.state.as_str(), "dirty");
    assert_eq!(delete_entry.sync_state.as_str(), "pending_local_changes");
    assert_eq!(delete_entry.issues[0].code, "pending_virtual_delete");

    let diff = run_diff(&store, &fixture.root).expect("diff virtual database row delete");
    let plan = diff
        .plan
        .as_ref()
        .expect("virtual database row delete plan");
    assert_eq!(diff.action, "confirm_plan");
    assert_eq!(plan.summary.entities_archived, 1, "{plan:#?}");
    assert_eq!(plan.affected_entities, vec![row.id.clone()]);

    let push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &fixture.root,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push virtual database row delete");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.journal_status.as_deref(), Some("reconciled"));
    assert_eq!(push.changed_remote_ids, vec![row.id.clone()]);
    assert_eq!(push.reconciled_remote_ids, vec![row.id.clone()]);
    assert_eq!(push.apply_effect_count, 1);

    let archived = cleanup
        .api
        .retrieve_page(&row.id)
        .expect("retrieve archived database row");
    assert!(
        archived.archived || archived.in_trash,
        "database row should be archived after virtual delete: {archived:#?}"
    );
    assert!(
        store
            .get_entity(&fixture.mount_id, &RemoteId::new(row.id.clone()))
            .expect("get deleted database row entity")
            .is_none(),
        "reconcile should remove archived database row entity from local state"
    );
    assert!(
        store
            .list_virtual_mutations(&fixture.mount_id)
            .expect("list mutations after database row delete push")
            .is_empty(),
        "reconcile should clear the pending database row delete mutation"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_virtual_page_directory_rename_updates_remote_title_and_reconciles() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live virtual rename parent {}", unique_suffix()),
        vec![paragraph_child("Parent body before child page rename.")],
    );
    let original_child_title = format!("Locality live virtual rename child {}", unique_suffix());
    let child = cleanup.create_page(
        &parent.id,
        &original_child_title,
        vec![paragraph_child("Child body before rename.")],
    );
    let connector = NotionConnector::new(
        live_notion_config().with_root_page_id(RemoteId::new(parent.id.clone())),
    );
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    mount_virtual_workspace(&fixture, &mut store, &parent.id);
    let content_root = fixture.content_root();
    let mount_point_root = fixture.virtual_root_identifier();
    refresh_virtual_fs_children(&mut store, &connector, &fixture.mount_id, &mount_point_root)
        .expect("refresh mount point root");
    let mount_point_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &mount_point_root,
    )
    .expect("list mount point root");
    let parent_folder = find_virtual_folder(&mount_point_children.children, &parent.id);
    refresh_virtual_fs_children(
        &mut store,
        &connector,
        &fixture.mount_id,
        &parent_folder.identifier,
    )
    .expect("refresh parent children");
    let parent_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &parent_folder.identifier,
    )
    .expect("list parent children");
    let child_folder = find_virtual_folder(&parent_children.children, &child.id);
    materialize_virtual_fs_item_with_content_root(
        &mut store,
        &connector,
        &content_root,
        &fixture.mount_id,
        &child.id,
    )
    .expect("hydrate child before rename");

    let renamed_child_title = format!("Locality live virtual renamed child {}", unique_suffix());
    let renamed = rename_virtual_fs_item(
        &mut store,
        &content_root,
        &fixture.mount_id,
        &child_folder.identifier,
        &parent_folder.identifier,
        &renamed_child_title,
    )
    .expect("rename child page directory");
    assert_eq!(renamed.identifier, child_folder.identifier);
    assert_eq!(renamed.item.filename, renamed_child_title);
    assert!(renamed.item.path.ends_with(&renamed_child_title));
    let renamed_page_path = fixture.root.join(&renamed.item.path).join("page.md");

    let pending = store
        .get_virtual_mutation(&fixture.mount_id, &format!("move:{}", child.id))
        .expect("get move mutation")
        .expect("move mutation");
    assert_eq!(
        pending.mutation_kind,
        locality_store::VirtualMutationKind::Move
    );
    assert_eq!(pending.title, renamed_child_title);

    let push = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &renamed_page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
        Some(&fixture.state_root),
    )
    .expect("push child page rename");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    assert_eq!(push.changed_remote_ids, vec![child.id.clone()]);
    assert!(
        store
            .get_virtual_mutation(&fixture.mount_id, &format!("rename:{}", child.id))
            .expect("get reconciled rename mutation")
            .is_none(),
        "rename mutation should be cleared after reconcile"
    );

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(renamed_page_path.clone()),
            state_root: Some(fixture.state_root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean rename status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let renamed_remote = render_live_markdown(&connector, &child.id, &renamed_page_path);
    assert!(
        renamed_remote.contains(&format!("title: \"{renamed_child_title}\"")),
        "{renamed_remote}"
    );
    assert!(renamed_remote.contains("Child body before rename."));
    let parent_remote = render_live_page(&connector, &parent.id, &renamed_page_path);
    assert!(
        parent_remote.contains(&renamed_child_title),
        "{parent_remote}"
    );
    assert!(
        !parent_remote.contains(&original_child_title),
        "{parent_remote}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_database_row_directory_create_pushes_row_and_reconciles() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live row directory scratch {}", unique_suffix()),
        Vec::new(),
    );
    let database = cleanup.create_database(
        &scratch.id,
        &format!("Locality live row directory database {}", unique_suffix()),
    );

    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let connector = NotionConnector::new(live_notion_config());
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(scratch.id.clone())),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live database row directory root page");
    run_pull(&mut store, &connector, &fixture.root).expect("pull live database row directory root");

    let database_dir = fixture.database_dir();
    let new_row_dir = database_dir.join("directory-created-row");
    fs::create_dir_all(&new_row_dir).expect("create row directory");
    let new_row_path = new_row_dir.join("page.md");
    fs::write(
        &new_row_path,
        "---\ntitle: Locality live directory created row\nNotes: \"Created from row directory\"\nPoints: 21\nStatus: Todo\nDone: false\n---\n# Directory row body\n\nCreated from database/new-row/page.md.\n",
    )
    .expect("write live database row directory page");

    let diff = run_diff(&store, &new_row_path).expect("diff database row directory create");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff
        .plan
        .as_ref()
        .expect("database row directory create plan");
    assert_eq!(plan.summary.entities_created, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &new_row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push database row directory create");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");
    let created_row_id = push
        .changed_remote_ids
        .iter()
        .find(|id| *id != &database.id)
        .expect("created database row id")
        .clone();
    cleanup.block_ids.push(created_row_id.clone());

    let row_entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new(created_row_id.clone()))
        .expect("get created row entity")
        .expect("created row entity");
    let reconciled_row_path = fixture.root.join(&row_entity.path);
    assert!(
        reconciled_row_path.exists(),
        "row directory create should reconcile to {}",
        reconciled_row_path.display()
    );
    assert_eq!(file_name(&reconciled_row_path), "page.md");
    let created_status = run_status(
        &store,
        StatusOptions {
            path: Some(reconciled_row_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("created row directory status");
    assert!(created_status.clean, "{created_status:#?}");

    let created = render_live_markdown(&connector, &created_row_id, &reconciled_row_path);
    for expected in [
        "title: \"Locality live directory created row\"",
        "\"Notes\": \"Created from row directory\"",
        "\"Points\": 21",
        "\"Status\": \"Todo\"",
        "\"Done\": false",
        "Created from database/new-row/page.md.",
    ] {
        assert!(
            created.contains(expected),
            "missing {expected:?}\n{created}"
        );
    }
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_database_row_invalid_select_option_blocks_before_journaled_apply() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live invalid row scratch {}", unique_suffix()),
        Vec::new(),
    );
    let database = cleanup.create_database(
        &scratch.id,
        &format!("Locality live invalid row database {}", unique_suffix()),
    );
    let row = cleanup.create_database_row(
        &database,
        &format!("Locality live invalid row {}", unique_suffix()),
        database_row_properties(
            "Initial validation row notes",
            "5",
            "Todo",
            "Not started",
            false,
            "https://example.com/loc-invalid-row",
            &[],
            &[],
        ),
        vec![paragraph_child("Validation row body.")],
    );

    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let connector = NotionConnector::new(live_notion_config());
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(scratch.id.clone())),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live invalid row root page");
    run_pull(&mut store, &connector, &fixture.root).expect("pull live invalid row root page");

    let row_entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new(row.id.clone()))
        .expect("get invalid row entity")
        .expect("invalid row entity");
    let row_path = fixture.root.join(&row_entity.path);
    run_pull(&mut store, &connector, &row_path).expect("hydrate live invalid row");
    let original = fs::read_to_string(&row_path).expect("read invalid row markdown");
    assert!(original.contains("\"Status\": \"Todo\""), "{original}");
    let before = live_page_snapshot(&connector, &row.id);
    fs::write(
        &row_path,
        original.replace("\"Status\": \"Todo\"", "\"Status\": \"Blocked\""),
    )
    .expect("write invalid select option");

    let diff = run_diff(&store, &row_path).expect("diff invalid select option");
    assert!(!diff.ok, "{diff:#?}");
    assert_eq!(diff.action, "fix_validation", "{diff:#?}");
    assert!(diff.plan.is_none(), "{diff:#?}");
    assert_eq!(diff.validation[0].code, "notion_schema_option_unknown");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push invalid select option");
    assert!(!push.ok, "{push:#?}");
    assert_eq!(push.action, "fix_validation", "{push:#?}");
    assert_eq!(push.push_id, None, "{push:#?}");
    assert_eq!(push.journal_status, None, "{push:#?}");
    assert!(store.list_journal().expect("journal").is_empty());
    assert_eq!(
        live_page_snapshot(&connector, &row.id),
        before,
        "invalid database row option must not mutate Notion"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_remote_fast_forward_updates_clean_file_and_preserves_pending_file() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live fast forward {}", unique_suffix()),
        vec![paragraph_child("Fast forward base body.")],
    );
    let connector = NotionConnector::new(
        live_notion_config().with_root_page_id(RemoteId::new(scratch.id.clone())),
    );
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    mount_virtual_workspace(&fixture, &mut store, &scratch.id);
    let content_root = fixture.content_root();
    hydrate_virtual_root_page(&fixture, &mut store, &connector, &content_root, &scratch.id);
    let page_path = content_root.join(
        store
            .get_entity(&fixture.mount_id, &RemoteId::new(scratch.id.clone()))
            .expect("get entity")
            .expect("entity")
            .path,
    );

    let clean_remote_marker = format!("Clean remote update {}", unique_suffix());
    append_remote_paragraph(&cleanup.api, &scratch.id, &clean_remote_marker);
    let clean_outcome =
        HydrationExecutor::new_with_output_root(&mut store, &connector, content_root.clone())
            .hydrate_request(HydrationRequest::new(
                fixture.mount_id.clone(),
                RemoteId::new(scratch.id.clone()),
                page_path.clone(),
                HydrationState::Hydrated,
                HydrationReason::RemoteFastForward,
            ))
            .expect("fast-forward clean file");
    assert_eq!(clean_outcome, HydrationOutcome::Hydrated);
    let clean_contents = fs::read_to_string(&page_path).expect("read fast-forwarded file");
    assert!(clean_contents.contains(&clean_remote_marker));

    let local_marker = format!("Local pending protected {}", unique_suffix());
    let remote_marker = format!("Remote update while pending {}", unique_suffix());
    fs::write(&page_path, format!("{clean_contents}\n\n{local_marker}\n"))
        .expect("write pending local edit");
    append_remote_paragraph(&cleanup.api, &scratch.id, &remote_marker);
    let protected_outcome =
        HydrationExecutor::new_with_output_root(&mut store, &connector, content_root.clone())
            .hydrate_request(HydrationRequest::new(
                fixture.mount_id.clone(),
                RemoteId::new(scratch.id.clone()),
                page_path.clone(),
                HydrationState::Hydrated,
                HydrationReason::RemoteFastForward,
            ))
            .expect("skip pending file");
    assert_eq!(protected_outcome, HydrationOutcome::SkippedDirty);
    let protected_contents = fs::read_to_string(&page_path).expect("read protected file");
    assert!(protected_contents.contains(&local_marker));
    assert!(
        !protected_contents.contains(&remote_marker),
        "pending local content must not be overwritten by remote fast-forward"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_scheduled_pull_queues_and_applies_remote_fast_forward() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live scheduled pull parent {}", unique_suffix()),
        vec![paragraph_child("Scheduler parent body.")],
    );
    let child = cleanup.create_page(
        &parent.id,
        &format!("Locality live scheduled pull child {}", unique_suffix()),
        vec![paragraph_child("Scheduler child base body.")],
    );
    let connector = NotionConnector::new(
        live_notion_config().with_root_page_id(RemoteId::new(parent.id.clone())),
    );
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(parent.id.clone())),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount scheduled pull workspace");
    let mounts = store.load_mounts().expect("load scheduled pull mounts");
    let strategy = DefaultFetchScheduleStrategy;
    let policy = HydrationPolicy::default();
    let mut scheduler = PullScheduler::new(Default::default());
    let mut queue = HydrationQueue::new();

    let initial_tick = scheduler.tick().expect("initial scheduled pull tick");
    let initial = reconcile_scheduled_pull(
        &mut store,
        &mut queue,
        &mounts,
        &initial_tick,
        &connector,
        &strategy,
        &policy,
    )
    .expect("initial live scheduled pull");
    assert_eq!(initial.mounts_polled, 1, "{initial:#?}");
    assert!(
        initial.enumerated >= 2,
        "scheduled pull should enumerate parent and child: {initial:#?}"
    );
    assert!(
        initial.queued_hydrations >= 1,
        "scheduled pull should queue root policy hydration: {initial:#?}"
    );
    HydrationExecutor::new(&mut store, &connector)
        .drain_queue(&mut queue)
        .expect("drain initial policy hydration");

    let child_entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new(child.id.clone()))
        .expect("get scheduled child entity")
        .expect("scheduled child entity");
    let child_relative_path = child_entity.path.clone();
    let child_path = fixture.root.join(&child_relative_path);
    let child_hydration = HydrationExecutor::new(&mut store, &connector)
        .hydrate_request(HydrationRequest::new(
            fixture.mount_id.clone(),
            RemoteId::new(child.id.clone()),
            child_relative_path.clone(),
            HydrationState::Hydrated,
            HydrationReason::ExplicitPull,
        ))
        .expect("hydrate scheduled child");
    assert_eq!(child_hydration, HydrationOutcome::Hydrated);
    let hydrated_child = fs::read_to_string(&child_path).expect("read hydrated scheduled child");
    assert!(
        hydrated_child.contains("Scheduler child base body."),
        "{hydrated_child}"
    );

    let remote_marker = format!("Scheduler remote fast-forward {}", unique_suffix());
    append_remote_paragraph_and_wait(
        &cleanup.api,
        &child.id,
        &remote_marker,
        "scheduled pull remote fast-forward",
    );
    // Notion can report page edit times with coarse precision; keep the test
    // focused on the scheduler path by making the local synced version stale.
    let mut stale_child = store
        .get_entity(&fixture.mount_id, &RemoteId::new(child.id.clone()))
        .expect("get scheduled child before remote update")
        .expect("scheduled child before remote update");
    stale_child.set_synced_tree_remote_version(Some("1970-01-01T00:00:00.000Z".to_string()));
    store
        .save_entity(stale_child)
        .expect("mark scheduled child synced version stale");

    let update_tick = scheduler
        .advance_by(Duration::from_secs(15))
        .expect("remote update scheduled pull tick");
    let update = reconcile_scheduled_pull(
        &mut store,
        &mut queue,
        &mounts,
        &update_tick,
        &connector,
        &strategy,
        &policy,
    )
    .expect("remote update scheduled pull");
    assert!(
        update.queued_hydrations >= 1,
        "remote update should queue hydration: {update:#?}"
    );

    let mut queued = Vec::new();
    while let Some(request) = queue.pop_ready() {
        queued.push(request);
    }
    assert!(
        queued.iter().any(|request| {
            request.remote_id == RemoteId::new(child.id.clone())
                && request.reason == HydrationReason::RemoteFastForward
        }),
        "scheduled pull should queue child remote fast-forward hydration: {queued:#?}"
    );
    for request in queued {
        queue.queue_request(request);
    }

    let drain = HydrationExecutor::new(&mut store, &connector)
        .drain_queue(&mut queue)
        .expect("drain scheduled remote fast-forward");
    assert!(
        drain.hydrated >= 1,
        "remote fast-forward should hydrate at least one page: {drain:#?}"
    );
    let updated_child = fs::read_to_string(&child_path).expect("read fast-forwarded child");
    assert!(updated_child.contains(&remote_marker), "{updated_child}");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_scheduled_pull_idle_ticks_do_not_repeat_notion_enumeration_or_queue_duplicates() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live scheduled idle parent {}", unique_suffix()),
        vec![paragraph_child("Scheduler idle parent body.")],
    );
    cleanup.create_page(
        &parent.id,
        &format!("Locality live scheduled idle child {}", unique_suffix()),
        vec![paragraph_child("Scheduler idle child body.")],
    );
    let connector = NotionConnector::new(
        live_notion_config().with_root_page_id(RemoteId::new(parent.id.clone())),
    );
    let source = CountingScheduledPullSource::new(&connector);
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(parent.id.clone())),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live scheduled idle workspace");
    let mounts = store
        .load_mounts()
        .expect("load live scheduled idle mounts");
    let strategy = DefaultFetchScheduleStrategy;
    let policy = HydrationPolicy::default();
    let mut scheduler = PullScheduler::new(locality_core::pull::PullSchedulerConfig {
        active_interval: Duration::from_secs(60),
        cold_interval: Duration::from_secs(600),
        ..Default::default()
    });
    let mut queue = HydrationQueue::new();

    let initial_tick = scheduler.tick().expect("initial live scheduled idle tick");
    assert!(!initial_tick.is_idle(), "{initial_tick:#?}");
    let initial = reconcile_scheduled_pull(
        &mut store,
        &mut queue,
        &mounts,
        &initial_tick,
        &source,
        &strategy,
        &policy,
    )
    .expect("initial live scheduled idle reconcile");
    assert_eq!(source.enumeration_count(), 1);
    assert_eq!(initial.mounts_polled, 1, "{initial:#?}");
    assert!(
        initial.enumerated >= 2,
        "scheduled pull should enumerate parent and child: {initial:#?}"
    );
    assert_eq!(
        queue.len(),
        1,
        "initial scheduled pull should leave one root hydration queued: {initial:#?}"
    );

    for tick_number in 1..=5 {
        let idle_tick = scheduler
            .advance_by(Duration::from_secs(1))
            .expect("live scheduled idle subinterval tick");
        assert!(
            idle_tick.is_idle(),
            "tick {tick_number} should be idle: {idle_tick:#?}"
        );
        let idle = reconcile_scheduled_pull(
            &mut store, &mut queue, &mounts, &idle_tick, &source, &strategy, &policy,
        )
        .expect("live scheduled idle reconcile");
        assert_eq!(source.enumeration_count(), 1);
        assert_eq!(idle.mounts_checked, 1, "{idle:#?}");
        assert_eq!(idle.mounts_polled, 0, "{idle:#?}");
        assert_eq!(idle.enumerated, 0, "{idle:#?}");
        assert_eq!(idle.queued_hydrations, 0, "{idle:#?}");
        assert_eq!(queue.len(), 1, "tick {tick_number}: {idle:#?}");
    }

    let due_tick = scheduler
        .advance_by(Duration::from_secs(55))
        .expect("live scheduled idle due tick");
    assert!(!due_tick.is_idle(), "{due_tick:#?}");
    let due = reconcile_scheduled_pull(
        &mut store, &mut queue, &mounts, &due_tick, &source, &strategy, &policy,
    )
    .expect("live scheduled idle due reconcile");
    assert_eq!(source.enumeration_count(), 2);
    assert_eq!(
        source.schema_count(),
        0,
        "page-only scheduled pulls should not fetch database schemas"
    );
    assert_eq!(due.mounts_polled, 1, "{due:#?}");
    assert!(
        due.enumerated >= 2,
        "due scheduled pull should enumerate parent and child: {due:#?}"
    );
    assert_eq!(
        queue.len(),
        1,
        "due scheduled pull should merge duplicate root hydration work: {due:#?}"
    );
}

#[test]
#[ignore = "requires Notion credentials in ~/.loc credentials and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_daemon_scheduler_polling_stays_within_notion_api_budget() {
    let env = LiveEnv::from_env();
    let source_connection_id =
        std::env::var(LIVE_CONNECTION_ENV).unwrap_or_else(|_| "notion-default".to_string());
    let stored_secret =
        live_notion_secret_from_default_store(&source_connection_id).expect("stored credential");
    let access_token =
        notion_access_token_from_secret(&stored_secret).expect("stored access token");
    let api = HttpNotionApi::new(NotionConfig::default().with_token(access_token.clone()));
    let mut cleanup = LiveCleanup::new(api);
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live daemon scheduler parent {}", unique_suffix()),
        vec![paragraph_child("Daemon scheduler parent body.")],
    );
    cleanup.create_page(
        &parent.id,
        &format!("Locality live daemon scheduler child {}", unique_suffix()),
        vec![paragraph_child("Daemon scheduler child body.")],
    );

    let fixture = E2eFixture::new();
    let connection_id = ConnectionId::new("stored-live-notion-daemon-scheduler");
    seed_cli_live_connection(&fixture.state_root, &connection_id, &stored_secret);
    let mut store =
        SqliteStateStore::open(fixture.state_root.clone()).expect("open live daemon store");
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(parent.id.clone())),
            connection_id: Some(connection_id),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live daemon scheduled workspace");
    drop(store);

    let connector = NotionConnector::new(
        NotionConfig::default()
            .with_token(access_token)
            .with_root_page_id(RemoteId::new(parent.id.clone())),
    );
    let runner = LiveDaemonScheduledPullRunner::new(connector);
    let mut config = localityd::DaemonConfig {
        state_root: fixture.state_root.clone(),
        runtime_tick_interval: Duration::from_millis(250),
        ..Default::default()
    };
    config.pull_scheduler.mode = locality_core::pull::PullMode::Polling;
    config.pull_scheduler.active_interval = Duration::from_secs(2);
    config.pull_scheduler.cold_interval = Duration::from_secs(60);

    let runtime = localityd::runtime::DaemonRuntime::spawn_with_runner(config, runner.clone())
        .expect("spawn live polling daemon runtime");
    let first = runner.wait_for_scheduled_count(1, Duration::from_secs(20));
    assert_eq!(
        first,
        1,
        "live daemon scheduler should complete the initial scheduled pull; last error: {:?}",
        runner.last_error()
    );
    let observed = runner.wait_for_scheduled_count(2, Duration::from_secs(30));
    runtime.shutdown();

    assert!(
        observed >= 2,
        "live daemon scheduler should run a second due poll, observed {observed}; last error: {:?}",
        runner.last_error()
    );
    assert!(
        observed <= 3,
        "live daemon scheduler exceeded the wall-clock active-interval budget: {observed}"
    );
    assert_eq!(
        runner.api_enumeration_count(),
        observed,
        "each scheduled daemon poll should enumerate the live Notion mount once"
    );
    for report in runner.reports() {
        assert_eq!(report.mounts_polled, 1, "{report:#?}");
        assert!(
            report.enumerated >= 2,
            "live daemon poll should enumerate the scratch parent and child: {report:#?}"
        );
        assert_eq!(
            report.queued_hydrations, 1,
            "duplicate root hydration should stay merged per daemon poll: {report:#?}"
        );
    }
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_locate_notion_url_returns_markdown_path_and_can_prioritize_hydration() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live locate {}", unique_suffix()),
        vec![paragraph_child(
            "Located page body should hydrate after URL lookup.",
        )],
    );
    let connector = NotionConnector::new(
        live_notion_config().with_root_page_id(RemoteId::new(scratch.id.clone())),
    );
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    mount_virtual_workspace(&fixture, &mut store, &scratch.id);
    let content_root = fixture.content_root();
    let mount_point_root = fixture.virtual_root_identifier();
    refresh_virtual_fs_children(&mut store, &connector, &fixture.mount_id, &mount_point_root)
        .expect("index mount point root");

    let url = notion_object_url(&scratch.id);
    let search = run_search(&store, SearchOptions::new(url)).expect("locate by Notion URL");
    let located = search
        .results
        .iter()
        .find(|result| compact_notion_id(&result.remote_id) == compact_notion_id(&scratch.id))
        .expect("located scratch page");
    assert_eq!(located.kind, "page");
    assert_eq!(located.state, "online_only");
    assert!(
        located.path.ends_with("/page.md"),
        "locate should return the page.md file path, not only a page directory: {located:#?}"
    );
    assert!(
        located.absolute_path.ends_with(&located.path),
        "{located:#?}"
    );

    let materialized = materialize_virtual_fs_item_with_content_root(
        &mut store,
        &connector,
        &content_root,
        &fixture.mount_id,
        &scratch.id,
    )
    .expect("hydrate located page");
    let markdown = fs::read_to_string(materialized.path).expect("read hydrated located page");
    assert!(markdown.contains("Located page body should hydrate after URL lookup."));
}

#[test]
#[ignore = "requires Notion credentials in ~/.loc credentials and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cli_locate_notion_url_prints_local_markdown_path_and_queues_hydration() {
    let env = LiveEnv::from_env();
    let source_connection_id =
        std::env::var(LIVE_CONNECTION_ENV).unwrap_or_else(|_| "notion-default".to_string());
    let stored_secret =
        live_notion_secret_from_default_store(&source_connection_id).expect("stored credential");
    let access_token =
        notion_access_token_from_secret(&stored_secret).expect("stored access token");
    let api = HttpNotionApi::new(NotionConfig::default().with_token(access_token));
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live CLI locate root {}", unique_suffix()),
        vec![paragraph_child("CLI locate root body.")],
    );
    let child_title = format!("Locality live CLI located child {}", unique_suffix());
    let child = cleanup.create_page(
        &scratch.id,
        &child_title,
        vec![paragraph_child(
            "CLI locate child body should be queued for hydration.",
        )],
    );

    let fixture = E2eFixture::new();
    let loc = env!("CARGO_BIN_EXE_loc");
    let connection_id = ConnectionId::new("stored-live-notion-locate");
    seed_cli_live_connection(&fixture.state_root, &connection_id, &stored_secret);
    let root = fixture.root.display().to_string();
    let mount = loc_json_ok(loc_command(loc, &fixture.state_root).args([
        "mount",
        "notion",
        root.as_str(),
        "--root-page",
        scratch.id.as_str(),
        "--connection",
        connection_id.as_str(),
        "--mount-id",
        fixture.mount_id.as_str(),
        "--projection",
        "plain-files",
        "--json",
    ]));
    assert_eq!(mount.value["ok"], true, "{mount:#?}");

    let child_url = notion_pretty_workspace_url("codeflash", &child_title, &child.id);
    let locate_stdout = loc_text_with_exit(
        loc_command(loc, &fixture.state_root).args(["locate", child_url.as_str()]),
        0,
    );
    assert!(
        locate_stdout.ends_with('\n'),
        "locate should print one newline-terminated path, got {locate_stdout:?}"
    );
    let located_lines = locate_stdout.lines().collect::<Vec<_>>();
    assert_eq!(
        located_lines.len(),
        1,
        "locate should print only the local path, got {locate_stdout:?}"
    );
    let located_path = PathBuf::from(located_lines[0]);
    assert!(
        located_path.starts_with(&fixture.root),
        "located path should be under the mounted root: {}",
        located_path.display()
    );
    assert_eq!(
        located_path.file_name().and_then(|name| name.to_str()),
        Some("page.md"),
        "locate should resolve page URLs to page.md: {}",
        located_path.display()
    );
    assert_eq!(
        located_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str()),
        Some(child_title.as_str()),
        "locate should preserve the child page title in the projected path: {}",
        located_path.display()
    );

    let store = SqliteStateStore::open(fixture.state_root.clone()).expect("open locate state");
    let located = store
        .get_entity(&fixture.mount_id, &RemoteId::new(child.id.clone()))
        .expect("load located child entity")
        .expect("located child entity");
    assert_eq!(
        located.path,
        located_path
            .strip_prefix(&fixture.root)
            .expect("located path under fixture root")
    );
    assert_eq!(located.hydration, HydrationState::Stub);
    let jobs = store.list_hydration_jobs().expect("list hydration jobs");
    assert!(
        jobs.iter().any(|job| {
            job.mount_id == fixture.mount_id
                && job.remote_id == RemoteId::new(child.id.clone())
                && job.path == located_path
                && job.reason == HydrationReason::FileOpen
        }),
        "locate should queue FileOpen hydration when localityd is disabled: {jobs:#?}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_remote_delete_observation_removes_clean_hydrated_plain_file_page() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live remote delete {}", unique_suffix()),
        vec![paragraph_child(
            "Clean page should disappear locally after remote archive.",
        )],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (fixture, mut store, page_path, markdown) = pull_live_page(&connector, &scratch.id);
    assert!(page_path.exists(), "live page was not materialized");
    assert!(
        markdown.contains("Clean page should disappear locally after remote archive."),
        "{markdown}"
    );

    cleanup
        .api
        .delete_block(&scratch.id)
        .expect("archive live scratch page");
    let observation = wait_for_live_deleted_observation(
        &connector,
        &fixture.mount_id,
        &scratch.id,
        "remote delete clean hydrated plain file",
    );

    apply_remote_observation(
        &mut store,
        observe_job(&fixture.mount_id, &RemoteId::new(scratch.id.clone())),
        observation,
    )
    .expect("apply live deleted observation");

    assert!(
        !page_path.exists(),
        "clean hydrated page should be removed after live remote delete"
    );
    assert!(
        page_path
            .parent()
            .is_some_and(|directory| !directory.exists()),
        "page directory should be removed after deleting page.md"
    );
    assert!(
        store
            .get_entity(&fixture.mount_id, &RemoteId::new(scratch.id.clone()))
            .expect("get deleted live entity")
            .is_none()
    );

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("live remote delete status");
    assert!(status.clean, "{status:#?}");
    assert_eq!(status.summary.total, 0, "{status:#?}");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_locate_new_child_page_then_parent_pull_projects_virtual_directory() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live child locate root {}", unique_suffix()),
        vec![paragraph_child(
            "Root page body before creating a fresh child page.",
        )],
    );
    let existing_child_title = format!("Locality live existing child {}", unique_suffix());
    let existing_child = cleanup.create_page(
        &scratch.id,
        &existing_child_title,
        vec![paragraph_child(
            "Existing child primes the cached parent listing.",
        )],
    );
    let connector = NotionConnector::new(
        live_notion_config().with_root_page_id(RemoteId::new(scratch.id.clone())),
    );
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    mount_virtual_workspace(&fixture, &mut store, &scratch.id);
    let content_root = fixture.content_root();

    let mount_point_root = fixture.virtual_root_identifier();
    refresh_virtual_fs_children(&mut store, &connector, &fixture.mount_id, &mount_point_root)
        .expect("refresh mount point root");
    let mount_point_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &mount_point_root,
    )
    .expect("list mount point root");
    let scratch_folder = find_virtual_folder(&mount_point_children.children, &scratch.id).clone();
    refresh_virtual_children_until_remote_folder(
        &mut store,
        &connector,
        &content_root,
        &fixture.mount_id,
        &scratch_folder.identifier,
        &existing_child.id,
    );
    let existing_child_url =
        notion_pretty_workspace_url("codeflash", &existing_child_title, &existing_child.id);
    let mut existing_locate_store = store.clone();
    let existing_child_path = desktop_style_locate_notion_url_path(
        &mut existing_locate_store,
        &connector,
        &fixture.mount_id,
        &existing_child_url,
    );
    let existing_child_dir_name = existing_child_path
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned());
    assert_eq!(
        existing_child_dir_name.as_deref(),
        Some(existing_child_title.as_str()),
        "existing child pretty URL should locate to its title-faithful projected page.md path: {existing_child_path:?}"
    );

    let child_title = format!("Locality live located child {}", unique_suffix());
    let child = cleanup.create_page(
        &scratch.id,
        &child_title,
        vec![paragraph_child(
            "Fresh child should appear after the parent directory is refreshed.",
        )],
    );
    let child_url = notion_pretty_workspace_url("codeflash", &child_title, &child.id);
    let mut locate_store = store.clone();
    let located_child_path = desktop_style_locate_notion_url_path(
        &mut locate_store,
        &connector,
        &fixture.mount_id,
        &child_url,
    );
    let parent_directory = located_child_path
        .parent()
        .and_then(Path::parent)
        .expect("located child parent directory");

    refresh_virtual_children_until_remote_folder(
        &mut store,
        &connector,
        &content_root,
        &fixture.mount_id,
        &scratch_folder.identifier,
        &child.id,
    );

    let pull = run_pull_with_state_root(
        &mut store,
        &connector,
        parent_directory,
        Some(&fixture.state_root),
    )
    .expect("pull parent directory after locating child URL");
    assert!(pull.ok, "{pull:#?}");
    assert!(
        pull.enumerated >= 2,
        "parent directory pull should enumerate existing and fresh child pages: {pull:#?}"
    );

    let parent_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &scratch_folder.identifier,
    )
    .expect("list refreshed parent children");
    let child_folder = find_virtual_folder(&parent_children.children, &child.id);
    let located_relative_path = located_child_path
        .strip_prefix(&fixture.root)
        .expect("located path under mount point root");
    assert_eq!(
        Path::new(&child_folder.path).join("page.md"),
        located_relative_path,
        "desktop-style locate and virtual directory projection should agree"
    );

    let child_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &child_folder.identifier,
    )
    .expect("list new child page directory");
    assert!(
        child_children.children.iter().any(|item| {
            item.filename == "page.md" && item.remote_id.as_deref() == Some(child.id.as_str())
        }),
        "new child directory should expose page.md after parent pull: {child_children:#?}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cyclic_diverse_page_read_noop_preserves_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let target = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality cyclic link target {}", unique_suffix()),
        vec![paragraph_child("Target page for live link checks.")],
    );
    let linked_database = cleanup.create_database(
        &env.parent_page_id,
        &format!("Locality cyclic linked database {}", unique_suffix()),
    );
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality cyclic diverse read {}", unique_suffix()),
        diverse_page_children(&target.id, &linked_database.id),
    );
    cleanup.create_page(
        &source.id,
        &format!("Locality cyclic nested child {}", unique_suffix()),
        vec![paragraph_child(
            "Nested child page for directory projection checks.",
        )],
    );

    let connector = NotionConnector::new(live_notion_config());
    let before = live_block_snapshot(&connector, &source.id);
    let (_fixture, mut store, page_path, markdown) = pull_live_page(&connector, &source.id);

    for expected in [
        "Cyclic paragraph",
        "# Cyclic heading one",
        "## Cyclic heading two",
        "### Cyclic heading three",
        "#### Cyclic heading four",
        "- Cyclic bullet",
        "1. Cyclic number",
        "- [ ] Cyclic todo",
        "> Cyclic quote",
        "> [!NOTE]\n> Cyclic callout",
        "```rust\nfn cyclic() {}\n```",
        "$$\na^2+b^2=c^2\n$$",
        "| Left | Right |",
        "[Linked page](https://www.notion.so/",
        "[Linked database](https://www.notion.so/",
        "target mention [Locality cyclic link target",
        "database mention [Locality cyclic linked database",
        "[Cyclic bookmark](https://example.com/cyclic-bookmark)",
        "[Cyclic embed](https://example.com/cyclic-embed)",
    ] {
        assert!(
            markdown.contains(expected),
            "missing {expected:?}\n{markdown}"
        );
    }
    assert_local_image_markdown(&markdown, "Cyclic image");
    assert_local_media_link_markdown(&markdown, "Cyclic video");
    assert_local_media_link_markdown(&markdown, "Cyclic file");
    assert_local_media_link_markdown(&markdown, "Cyclic PDF");
    assert_local_media_link_markdown(&markdown, "Cyclic audio");
    assert!(
        !markdown.contains("type=link_to_page"),
        "link_to_page should render as a Markdown link, not a directive:\n{markdown}"
    );

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("noop push");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "noop", "{push:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    assert_eq!(
        after, before,
        "read/noop cyclic path must not modify Notion block JSON"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_code_block_with_embedded_fence_edits_round_trip() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live embedded code fence {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "code",
            "code": {
                "rich_text": rich_text_json("Before\n```python\nprint('nested')\n```\nAfter"),
                "language": "markdown",
            }
        })],
    );

    let connector = NotionConnector::new(live_notion_config());
    let (fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let expected_original = "````markdown\nBefore\n```python\nprint('nested')\n```\nAfter\n````";
    assert!(
        original.contains(expected_original),
        "code block should render with a longer outer fence:\n{original}"
    );

    fs::write(
        &page_path,
        original.replace("After\n````", "After updated\n````"),
    )
    .expect("write edited embedded code fence");

    let diff = run_diff(&store, &page_path).expect("diff embedded code fence edit");
    let plan = diff.plan.as_ref().expect("plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push embedded code fence edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let verified = render_live_page(&connector, &source.id, &page_path);
    assert!(
        verified
            .contains("````markdown\nBefore\n```python\nprint('nested')\n```\nAfter updated\n````"),
        "verified markdown should preserve the embedded fence:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_code_block_ignores_fence_marker_with_trailing_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live code false closer {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "code",
            "code": {
                "rich_text": rich_text_json("Before"),
                "language": "markdown",
            }
        })],
    );

    let connector = NotionConnector::new(live_notion_config());
    let (fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    assert!(
        original.contains("```markdown\nBefore\n```"),
        "code block should render as a simple fence:\n{original}"
    );

    fs::write(
        &page_path,
        original.replace(
            "Before\n```",
            "Before\n```not a closing fence\nAfter updated\n```",
        ),
    )
    .expect("write edited false closing fence");

    let diff = run_diff(&store, &page_path).expect("diff false closing fence edit");
    let plan = diff.plan.as_ref().expect("plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");
    assert_eq!(plan.summary.blocks_created, 0, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push false closing fence edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let verified = render_live_page(&connector, &source.id, &page_path);
    assert!(
        verified.contains("````markdown\nBefore\n```not a closing fence\nAfter updated\n````"),
        "verified markdown should keep the false closer inside the code block:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_text_code_fence_alias_pushes_as_plain_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live text code alias {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "code",
            "code": {
                "rich_text": rich_text_json("Before"),
                "language": "plain text",
            }
        })],
    );

    let connector = NotionConnector::new(live_notion_config());
    let (fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    assert!(
        original.contains("```plain text\nBefore\n```"),
        "code block should render with Notion's plain text language:\n{original}"
    );

    fs::write(
        &page_path,
        original.replace("```plain text\nBefore\n```", "```text\nAfter alias\n```"),
    )
    .expect("write text code alias edit");

    let diff = run_diff(&store, &page_path).expect("diff text code alias edit");
    let plan = diff.plan.as_ref().expect("plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push text code alias edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let snapshot = live_block_snapshot(&connector, &source.id);
    let block = snapshot
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("live code block after text alias edit");
    assert_eq!(block["block"]["type"], "code");
    assert_eq!(block["block"]["code"]["language"], "plain text");

    let verified = render_live_page(&connector, &source.id, &page_path);
    assert!(
        verified.contains("```plain text\nAfter alias\n```"),
        "verified markdown should render the normalized plain text language:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cyclic_supported_block_edits_push_and_verify_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let user_id = cleanup.current_user_id();
    let target = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality cyclic supported link target {}", unique_suffix()),
        vec![paragraph_child("Target page for supported edit links.")],
    );
    let linked_database = cleanup.create_database(
        &env.parent_page_id,
        &format!(
            "Locality cyclic supported linked database {}",
            unique_suffix()
        ),
    );
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality cyclic supported edits {}", unique_suffix()),
        supported_edit_children(&user_id, &target.id, &linked_database.id),
    );

    let connector = NotionConnector::new(live_notion_config());
    let (fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let editable_image_line = markdown_image_line(&original, "Editable image");
    let editable_image_href = markdown_link_href(editable_image_line);
    let edited_image_line = format!("![Editable image changed]({editable_image_href})");
    let editable_video_line = markdown_link_line(&original, "Editable video");
    let editable_file_line = markdown_link_line(&original, "Editable file");
    let editable_pdf_line = markdown_link_line(&original, "Editable PDF");
    let editable_audio_line = markdown_link_line(&original, "Editable audio");
    let edited = original
        .replace(
            "Editable paragraph original.",
            "Editable paragraph changed.",
        )
        .replace("Editable date 2026-06-13", "Editable date @date(2026-06-14)")
        .replace("# Editable heading one", "# Editable heading one changed")
        .replace("## Editable heading two", "## Editable heading two changed")
        .replace(
            "### Editable heading three",
            "### Editable heading three changed",
        )
        .replace(
            "#### Editable heading four",
            "#### Editable heading four changed",
        )
        .replace("- Editable bullet", "- Editable bullet changed")
        .replace("1. Editable number", "1. Editable number changed")
        .replace("- [ ] Editable todo", "- [x] Editable todo changed")
        .replace("> Editable quote", "> Editable quote changed")
        .replace(
            "> [!NOTE]\n> Editable callout",
            "> [!NOTE]\n> Editable callout changed",
        )
        .replace(
            "| Editable table item | Editable table state |",
            "| Editable table item changed | Editable table state done |\n| Editable table added | Editable table added state |",
        )
        .replace(
            "[Editable bookmark](https://example.com/editable-bookmark)",
            "[Editable bookmark changed](https://example.com/editable-bookmark-changed)",
        )
        .replace(
            "[Editable embed](https://example.com/editable-embed)",
            "[Editable embed changed](https://example.com/editable-embed-changed)",
        )
        .replace(editable_image_line, &edited_image_line)
        .replace(
            editable_video_line,
            "[Editable video changed](https://www.youtube.com/watch?v=oHg5SJYRHA0)",
        )
        .replace(
            editable_file_line,
            "[Editable file changed](https://www.orimi.com/pdf-test.pdf)",
        )
        .replace(
            editable_pdf_line,
            "[Editable PDF changed](https://www.orimi.com/pdf-test.pdf)",
        )
        .replace(
            editable_audio_line,
            "[Editable audio changed](https://www.soundhelix.com/examples/mp3/SoundHelix-Song-2.mp3)",
        )
        .replace("fn editable() {}", "fn editable_changed() {}")
        .replace("x+y=z", "x-y=z");
    let edited = replace_line_with_prefix(
        edited,
        "Editable user ",
        &format!("Editable user @user({user_id})"),
    );
    let edited = replace_line_with_prefix(
        edited,
        "Editable typed links ",
        &format!(
            "Editable typed links @page({}) and @database({})",
            target.id, linked_database.id
        ),
    );
    fs::write(&page_path, edited).expect("write cyclic edits");

    let dirty_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("dirty status");
    assert!(!dirty_status.clean, "{dirty_status:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push cyclic edits");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let verified = render_live_page(&connector, &source.id, &page_path);
    let target_url = notion_object_url(&target.id);
    let linked_database_url = notion_object_url(&linked_database.id);
    for expected in [
        "Editable paragraph changed.",
        "Editable date 2026-06-14",
        "Editable user ",
        "Editable typed links ",
        target_url.as_str(),
        linked_database_url.as_str(),
        "# Editable heading one changed",
        "## Editable heading two changed",
        "### Editable heading three changed",
        "#### Editable heading four changed",
        "- Editable bullet changed",
        "1. Editable number changed",
        "- [x] Editable todo changed",
        "> Editable quote changed",
        "> [!NOTE]\n> Editable callout changed",
        "| Editable table item changed | Editable table state done |",
        "| Editable table added | Editable table added state |",
        "[Editable bookmark changed](https://example.com/editable-bookmark-changed)",
        "[Editable embed changed](https://example.com/editable-embed-changed)",
        "fn editable_changed() {}",
        "x-y=z",
    ] {
        assert!(
            verified.contains(expected),
            "missing {expected:?}\n{verified}"
        );
    }
    assert_local_image_markdown(&verified, "Editable image changed");
    assert_local_media_link_markdown(&verified, "Editable video changed");
    assert_local_media_link_markdown(&verified, "Editable file changed");
    assert_local_media_link_markdown(&verified, "Editable PDF changed");
    assert_local_media_link_markdown(&verified, "Editable audio changed");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_rich_text_markdown_pushes_annotations_links_equations_and_mentions() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let user_id = cleanup.current_user_id();
    let target = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality rich text target {}", unique_suffix()),
        vec![paragraph_child("Target page for rich text mention.")],
    );
    let linked_database = cleanup.create_database(
        &env.parent_page_id,
        &format!("Locality rich text database {}", unique_suffix()),
    );
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live rich text spans {}", unique_suffix()),
        vec![paragraph_child("Original rich text spans.")],
    );

    let connector = NotionConnector::new(live_notion_config());
    let (fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let edited_line = format!(
        "**Bold changed** _italic changed_ ~~strike changed~~ <u>underline changed</u> `code changed` [external changed](https://example.com/rich-text) $E=mc^2$ @date(2026-06-15) @user({user_id}) @page({}) @database({})",
        target.id, linked_database.id
    );
    fs::write(
        &page_path,
        original.replace("Original rich text spans.", &edited_line),
    )
    .expect("write live rich text span edit");

    let diff = run_diff(&store, &page_path).expect("diff live rich text span edit");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("rich text span edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push live rich text span edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean status after rich text span push");
    assert!(clean_status.clean, "{clean_status:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after rich text span push");
    assert_eq!(first["block"]["type"], "paragraph", "{after:#?}");
    let rich_text = first["block"]["paragraph"]["rich_text"]
        .as_array()
        .expect("live rich text array");
    let text_part = |content: &str| {
        rich_text
            .iter()
            .find(|part| part["type"] == "text" && part["text"]["content"] == content)
            .unwrap_or_else(|| panic!("missing text part `{content}` in {after:#?}"))
    };
    assert_eq!(text_part("Bold changed")["annotations"]["bold"], true);
    assert_eq!(text_part("italic changed")["annotations"]["italic"], true);
    assert_eq!(
        text_part("strike changed")["annotations"]["strikethrough"],
        true
    );
    assert_eq!(
        text_part("underline changed")["annotations"]["underline"],
        true
    );
    assert_eq!(text_part("code changed")["annotations"]["code"], true);
    assert_eq!(
        text_part("external changed")["text"]["link"]["url"],
        "https://example.com/rich-text"
    );
    assert!(
        rich_text
            .iter()
            .any(|part| part["type"] == "equation" && part["equation"]["expression"] == "E=mc^2"),
        "missing inline equation part in {after:#?}"
    );
    assert!(
        rich_text.iter().any(|part| {
            part["type"] == "mention"
                && part["mention"]["type"] == "date"
                && part["mention"]["date"]["start"] == "2026-06-15"
        }),
        "missing date mention in {after:#?}"
    );
    assert!(
        rich_text.iter().any(|part| {
            part["type"] == "mention"
                && part["mention"]["type"] == "user"
                && part["mention"]["user"]["id"] == user_id
        }),
        "missing user mention in {after:#?}"
    );
    assert!(
        rich_text.iter().any(|part| {
            part["type"] == "mention"
                && part["mention"]["type"] == "page"
                && part["mention"]["page"]["id"]
                    .as_str()
                    .is_some_and(|id| compact_notion_id(id) == compact_notion_id(&target.id))
        }),
        "missing page mention in {after:#?}"
    );
    assert!(
        rich_text.iter().any(|part| {
            part["type"] == "mention"
                && part["mention"]["type"] == "database"
                && part["mention"]["database"]["id"]
                    .as_str()
                    .is_some_and(|id| {
                        compact_notion_id(id) == compact_notion_id(&linked_database.id)
                    })
        }),
        "missing database mention in {after:#?}"
    );

    let verified = render_live_page(&connector, &source.id, &page_path);
    let target_url = notion_object_url(&target.id);
    let linked_database_url = notion_object_url(&linked_database.id);
    for expected in [
        "**Bold changed**",
        "_italic changed_",
        "~~strike changed~~",
        "<u>underline changed</u>",
        "`code changed`",
        "[external changed](https://example.com/rich-text)",
        "$E=mc^2$",
        "2026-06-15",
        target_url.as_str(),
        linked_database_url.as_str(),
    ] {
        assert!(
            verified.contains(expected),
            "missing rendered rich text marker {expected:?}\n{verified}"
        );
    }
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_rich_text_color_annotations_survive_adjacent_markdown_edit() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live rich text color {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": [
                    text_part("Prefix original "),
                    colored_text_part("Colored marker", "red"),
                    text_part(" suffix original.")
                ]
            }
        })],
    );

    let connector = NotionConnector::new(live_notion_config());
    let (fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    let original_line = "Prefix original Colored marker suffix original.";
    assert!(
        original.contains(original_line),
        "live colored rich text did not render as editable Markdown\n{original}"
    );
    fs::write(
        &page_path,
        original.replace(
            original_line,
            "Prefix changed Colored marker suffix changed.",
        ),
    )
    .expect("write adjacent edit around colored rich text");

    let diff = run_diff(&store, &page_path).expect("diff adjacent colored rich text edit");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("colored rich text edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push adjacent colored rich text edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean status after colored rich text push");
    assert!(clean_status.clean, "{clean_status:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after colored rich text push");
    assert_eq!(first["block"]["type"], "paragraph", "{after:#?}");
    let rich_text = first["block"]["paragraph"]["rich_text"]
        .as_array()
        .expect("live colored rich text array");
    let plain_text = rich_text
        .iter()
        .filter_map(|part| part["plain_text"].as_str())
        .collect::<String>();
    assert_eq!(
        plain_text, "Prefix changed Colored marker suffix changed.",
        "{after:#?}"
    );
    let colored = rich_text
        .iter()
        .find(|part| part["type"] == "text" && part["text"]["content"] == "Colored marker")
        .unwrap_or_else(|| panic!("missing colored text part in {after:#?}"));
    assert_eq!(colored["annotations"]["color"], "red", "{after:#?}");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_block_color_survives_mounted_markdown_text_edit() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live block color {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": rich_text_json("Colored block original."),
                "color": "yellow_background"
            }
        })],
    );

    let connector = NotionConnector::new(live_notion_config());
    let (fixture, mut store, page_path, original) = pull_live_page(&connector, &source.id);
    assert!(
        original.contains("Colored block original."),
        "live block color paragraph did not render as editable Markdown\n{original}"
    );
    fs::write(
        &page_path,
        original.replace("Colored block original.", "Colored block changed."),
    )
    .expect("write colored block text edit");

    let diff = run_diff(&store, &page_path).expect("diff colored block text edit");
    assert_eq!(diff.action, "confirm_plan", "{diff:#?}");
    let plan = diff.plan.as_ref().expect("colored block text edit plan");
    assert_eq!(plan.summary.blocks_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push colored block text edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean status after colored block push");
    assert!(clean_status.clean, "{clean_status:#?}");

    let after = live_block_snapshot(&connector, &source.id);
    let first = after
        .as_array()
        .and_then(|blocks| blocks.first())
        .expect("first live block after colored block push");
    assert_eq!(first["block"]["type"], "paragraph", "{after:#?}");
    assert_eq!(
        first["block"]["paragraph"]["color"], "yellow_background",
        "{after:#?}"
    );
    let rich_text = first["block"]["paragraph"]["rich_text"]
        .as_array()
        .expect("live colored block rich text array");
    let plain_text = rich_text
        .iter()
        .filter_map(|part| part["plain_text"].as_str())
        .collect::<String>();
    assert_eq!(plain_text, "Colored block changed.", "{after:#?}");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_local_image_media_edit_uploads_and_reconciles_bytes() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live local image {}", unique_suffix()),
        vec![media_child(
            "image",
            "https://www.w3.org/Icons/w3c_home.png",
            "Original local image",
        )],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);
    assert_local_image_markdown(&original, "Original local image");

    let image_path = local_image_path(&fixture.root, &page_path, &original, "Original local image");
    assert!(
        image_path.is_file(),
        "missing local image at {image_path:?}"
    );
    let uploaded_bytes = tiny_png_bytes();
    fs::write(&image_path, uploaded_bytes).expect("overwrite local image bytes");

    let original_image_line = markdown_image_line(&original, "Original local image");
    let image_href = markdown_link_href(original_image_line);
    let updated_image_line = format!("![Updated local image]({image_href})");
    fs::write(
        &page_path,
        original.replace(original_image_line, &updated_image_line),
    )
    .expect("write local image markdown edit");

    let diff = run_diff(&store, &page_path).expect("diff local image edit");
    let plan = diff.plan.as_ref().expect("image edit plan");
    assert_eq!(diff.action, "confirm_plan");
    assert_eq!(plan.summary.media_updated, 1, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push local image edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean image status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let reconciled = fs::read_to_string(&page_path).expect("read reconciled image page");
    assert_local_image_markdown(&reconciled, "Updated local image");
    let reconciled_image_path = local_image_path(
        &fixture.root,
        &page_path,
        &reconciled,
        "Updated local image",
    );
    assert_eq!(
        fs::read(&reconciled_image_path).expect("read reconciled image"),
        uploaded_bytes
    );

    let verified = render_live_page(&connector, &scratch.id, &page_path);
    assert_local_image_markdown(&verified, "Updated local image");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_local_image_media_edit_with_escaped_caption_uploads_and_reconciles() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live escaped local image {}", unique_suffix()),
        vec![media_child(
            "image",
            "https://www.w3.org/Icons/w3c_home.png",
            "Original escaped image",
        )],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);
    assert_local_image_markdown(&original, "Original escaped image");

    let image_path = local_image_path(
        &fixture.root,
        &page_path,
        &original,
        "Original escaped image",
    );
    assert!(
        image_path.is_file(),
        "missing local image at {image_path:?}"
    );
    let uploaded_bytes = tiny_png_bytes();
    fs::write(&image_path, uploaded_bytes).expect("overwrite local image bytes");

    let original_image_line = markdown_image_line(&original, "Original escaped image");
    let image_href = markdown_link_href(original_image_line);
    let escaped_caption = "Updated \\](escaped image)";
    let updated_image_line = format!("![{escaped_caption}]({image_href})");
    fs::write(
        &page_path,
        original.replace(original_image_line, &updated_image_line),
    )
    .expect("write local image markdown edit");

    let diff = run_diff(&store, &page_path).expect("diff escaped local image edit");
    let plan = diff.plan.as_ref().expect("escaped image edit plan");
    assert_eq!(diff.action, "confirm_plan");
    assert_eq!(plan.summary.media_updated, 1, "{plan:#?}");
    assert_eq!(plan.summary.blocks_updated, 0, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push escaped local image edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let reconciled = fs::read_to_string(&page_path).expect("read reconciled image page");
    let reconciled_line = reconciled
        .lines()
        .find(|line| line.starts_with(&format!("![{escaped_caption}](")))
        .unwrap_or_else(|| panic!("missing escaped image caption in:\n{reconciled}"));
    assert_local_media_href(reconciled_line, "Updated ](escaped image)");
    let reconciled_image_path =
        local_media_path_from_line(&fixture.root, &page_path, reconciled_line);
    assert_eq!(
        fs::read(&reconciled_image_path).expect("read reconciled image"),
        uploaded_bytes
    );

    let verified = render_live_page(&connector, &scratch.id, &page_path);
    assert!(
        verified.contains(&format!("![{escaped_caption}](")),
        "verified markdown should keep escaped caption:\n{verified}"
    );
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_local_file_like_media_appends_upload_and_reconcile_local_links() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality live local files {}", unique_suffix()),
        vec![paragraph_child("Base body before local file-like uploads.")],
    );
    let connector = NotionConnector::new(live_notion_config());
    let (fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);
    let media_dir = fixture
        .root
        .join(".loc")
        .join("media")
        .join(format!("live-local-files-{}", unique_suffix()));
    fs::create_dir_all(&media_dir).expect("create local file media dir");
    let video_path = media_dir.join("cars.mp4");
    let pdf_path = media_dir.join("brief.pdf");
    let audio_path = media_dir.join("theme.wav");
    let html_path = media_dir.join("index.html");
    fs::write(&video_path, tiny_mp4_bytes()).expect("write local video");
    fs::write(&pdf_path, tiny_pdf_bytes()).expect("write local pdf");
    fs::write(&audio_path, tiny_wav_bytes()).expect("write local audio");
    fs::write(&html_path, tiny_html_bytes()).expect("write local html");

    let edited = format!(
        "{original}\n[Uploaded video]({})\n\n[Uploaded PDF]({})\n\n[Uploaded audio]({})\n\n[Uploaded HTML]({})\n",
        video_path.display(),
        pdf_path.display(),
        audio_path.display(),
        html_path.display()
    );
    fs::write(&page_path, edited).expect("write file-like media append markdown");

    let diff = run_diff(&store, &page_path).expect("diff file-like media append");
    let plan = diff.plan.as_ref().expect("file-like media append plan");
    assert_eq!(diff.action, "confirm_plan");
    assert_eq!(plan.summary.blocks_created, 4, "{plan:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &page_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push file-like media append");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(page_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean file-like media status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let reconciled = fs::read_to_string(&page_path).expect("read reconciled file-like media page");
    for caption in [
        "Uploaded video",
        "Uploaded PDF",
        "Uploaded audio",
        "Uploaded HTML",
    ] {
        assert_local_media_link_markdown(&reconciled, caption);
        let local_path = local_media_link_path(&fixture.root, &page_path, &reconciled, caption);
        assert!(
            local_path.is_file(),
            "reconciled local media path should exist for {caption:?}: {local_path:?}"
        );
        assert!(
            !fs::read(&local_path)
                .expect("read reconciled local media")
                .is_empty(),
            "reconciled local media path should be non-empty for {caption:?}: {local_path:?}"
        );
    }

    let verified = render_live_page(&connector, &scratch.id, &page_path);
    assert_local_media_link_markdown(&verified, "Uploaded video");
    assert_local_media_link_markdown(&verified, "Uploaded PDF");
    assert_local_media_link_markdown(&verified, "Uploaded audio");
    assert_local_media_link_markdown(&verified, "Uploaded HTML");
}

#[test]
#[ignore = "requires Notion credentials (NOTION_TOKEN or ~/.loc credentials) and LOCALITY_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cyclic_database_rows_mount_edit_create_and_verify_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(live_notion_config());
    let mut cleanup = LiveCleanup::new(api);
    let people_user_id = cleanup.current_user_id();
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("Locality cyclic database scratch {}", unique_suffix()),
        Vec::new(),
    );
    let related_database = cleanup.create_database(
        &scratch.id,
        &format!("Locality cyclic related rows {}", unique_suffix()),
    );
    let related_data_source_id = related_database
        .data_sources
        .first()
        .expect("related data source")
        .id
        .clone();
    let related_row = cleanup.create_database_row(
        &related_database,
        &format!("Locality cyclic related row {}", unique_suffix()),
        serde_json::Map::new(),
        vec![paragraph_child("Related row target.")],
    );
    let database = cleanup.create_database_with_relation(
        &scratch.id,
        &format!("Locality cyclic rows {}", unique_suffix()),
        &related_data_source_id,
    );
    let existing_row = cleanup.create_database_row(
        &database,
        &format!("Locality cyclic existing row {}", unique_suffix()),
        database_row_properties(
            "Initial row notes",
            "7",
            "Todo",
            "Not started",
            false,
            "https://example.com/loc-db-row",
            &[],
            &[related_row.id.as_str()],
        ),
        vec![paragraph_child("Database row paragraph original.")],
    );

    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let connector = NotionConnector::new(live_notion_config());
    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(scratch.id.clone())),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live database root page");
    run_pull(&mut store, &connector, &fixture.root).expect("pull live database root page");

    let schema_path = fixture.schema_file();
    let schema = fs::read_to_string(&schema_path).expect("read live database schema");
    for expected in [
        "type: notion_database_schema",
        "\"Notes\":",
        "\"Points\":",
        "\"Status\":",
        "\"State\":",
        "\"Tags\":",
        "\"Done\":",
        "\"Due\":",
        "\"URL\":",
        "\"Email\":",
        "\"Phone\":",
        "\"Files\":",
        "\"People\":",
        "\"Related\":",
    ] {
        assert!(schema.contains(expected), "missing {expected:?}\n{schema}");
    }

    let row_path = fixture.nested_markdown_file_containing("Locality cyclic existing row");
    run_pull(&mut store, &connector, &row_path).expect("hydrate live database row");
    let original = fs::read_to_string(&row_path).expect("read hydrated row markdown");
    for expected in [
        "title: \"Locality cyclic existing row",
        "\"Notes\": \"Initial row notes\"",
        "\"Points\": 7",
        "\"Status\": \"Todo\"",
        "\"State\": \"Not started\"",
        "\"Done\": false",
        "\"URL\": \"https://example.com/loc-db-row\"",
        "\"Files\":",
        "\"Initial file <https://example.com/initial.pdf>\"",
        "\"People\": []",
        "\"Related\":",
        &format!("\"{}\"", related_row.id),
        "Database row paragraph original.",
    ] {
        assert!(
            original.contains(expected),
            "missing {expected:?}\n{original}"
        );
    }

    let before = live_page_snapshot(&connector, &existing_row.id);
    let clean_status = run_status(
        &store,
        StatusOptions {
            path: Some(row_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("clean row status");
    assert!(clean_status.clean, "{clean_status:#?}");

    let noop = run_push_with_daemon(
        &mut store,
        &connector,
        &row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("noop database row push");
    assert!(noop.ok, "{noop:#?}");
    assert_eq!(noop.action, "noop", "{noop:#?}");
    assert_eq!(
        live_page_snapshot(&connector, &existing_row.id),
        before,
        "read/noop database row cycle must not mutate Notion"
    );

    let edited = original
        .replace(
            "\"Notes\": \"Initial row notes\"",
            "\"Notes\": \"**Updated** row notes and @date(2026-06-14)\"",
        )
        .replace("\"Points\": 7", "\"Points\": 8")
        .replace("\"Status\": \"Todo\"", "\"Status\": \"Done\"")
        .replace("\"State\": \"Not started\"", "\"State\": \"In progress\"")
        .replace("\"Done\": false", "\"Done\": true")
        .replace(
            "\"URL\": \"https://example.com/loc-db-row\"",
            "\"URL\": \"https://example.com/loc-db-row-updated\"",
        )
        .replace(
            "\"Initial file <https://example.com/initial.pdf>\"",
            "\"Updated file <https://example.com/updated.pdf>\"",
        )
        .replace(
            "\"People\": []",
            &format!("\"People\":\n  - \"{}\"", people_user_id),
        )
        .replace(
            "Database row paragraph original.",
            "Database row paragraph changed.",
        );
    fs::write(&row_path, edited).expect("write live database row edit");
    let dirty_status = run_status(
        &store,
        StatusOptions {
            path: Some(row_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("dirty row status");
    assert!(!dirty_status.clean, "{dirty_status:#?}");

    let push = run_push_with_daemon(
        &mut store,
        &connector,
        &row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push database row edit");
    assert!(push.ok, "{push:#?}");
    assert_eq!(push.action, "reconciled", "{push:#?}");

    let verified = render_live_markdown(&connector, &existing_row.id, &row_path);
    for expected in [
        "\"Notes\": \"**Updated** row notes and 2026-06-14\"",
        "\"Points\": 8",
        "\"Status\": \"Done\"",
        "\"State\": \"In progress\"",
        "\"Done\": true",
        "\"URL\": \"https://example.com/loc-db-row-updated\"",
        "\"Updated file <https://example.com/updated.pdf>\"",
        &people_user_id,
        &format!("\"{}\"", related_row.id),
        "Database row paragraph changed.",
    ] {
        assert!(
            verified.contains(expected),
            "missing {expected:?}\n{verified}"
        );
    }

    let database_dir = fixture.database_dir();
    let new_row_path = database_dir.join("new-cyclic-row.md");
    fs::write(
        &new_row_path,
        format!(
            "---\ntitle: Locality cyclic created row\nNotes: \"Created **row** notes and [docs](https://example.com/created-notes)\"\nPoints: 13\nStatus: Todo\nState: Not started\nTags:\n  - Alpha\nDone: false\nDue: \"2026-06-13\"\nURL: https://example.com/loc-created-row\nEmail: cyclic@example.com\nPhone: \"+1 415 555 0199\"\nFiles:\n  - Created file <https://example.com/created.pdf>\nPeople:\n  - \"{}\"\nRelated:\n  - \"{}\"\n---\n# Created row body\n\nCreated from mounted markdown.\n",
            people_user_id, related_row.id
        ),
    )
    .expect("write new live database row file");

    let create_push = run_push_with_daemon(
        &mut store,
        &connector,
        &new_row_path,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .expect("push new database row");
    assert!(create_push.ok, "{create_push:#?}");
    assert_eq!(create_push.action, "reconciled", "{create_push:#?}");
    let created_row_id = create_push
        .changed_remote_ids
        .iter()
        .find(|id| *id != &database.id)
        .expect("created row id")
        .clone();
    cleanup.block_ids.push(created_row_id.clone());
    let reconciled_row_path = database_dir.join("new-cyclic-row").join("page.md");
    assert!(
        reconciled_row_path.exists(),
        "direct database row create should reconcile to a page document at {}",
        reconciled_row_path.display()
    );
    assert!(
        !new_row_path.exists(),
        "temporary direct row file should be replaced by the canonical page document"
    );
    let created_status = run_status(
        &store,
        StatusOptions {
            path: Some(reconciled_row_path.clone()),
            ..StatusOptions::default()
        },
    )
    .expect("created row status");
    assert!(created_status.clean, "{created_status:#?}");

    let created = render_live_markdown(&connector, &created_row_id, &reconciled_row_path);
    for expected in [
        "title: \"Locality cyclic created row\"",
        "\"Notes\": \"Created **row** notes and [docs](https://example.com/created-notes)\"",
        "\"Points\": 13",
        "\"Status\": \"Todo\"",
        "\"State\": \"Not started\"",
        "\"Tags\":",
        "\"Alpha\"",
        "\"Done\": false",
        "\"URL\": \"https://example.com/loc-created-row\"",
        "\"Email\": \"cyclic@example.com\"",
        "\"Phone\": \"+1 415 555 0199\"",
        "\"Created file <https://example.com/created.pdf>\"",
        &people_user_id,
        &format!("\"{}\"", related_row.id),
        "Created from mounted markdown.",
    ] {
        assert!(
            created.contains(expected),
            "missing {expected:?}\n{created}"
        );
    }
}

struct E2eFixture {
    root: PathBuf,
    state_root: PathBuf,
    mount_id: MountId,
}

impl E2eFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "loc-cli-e2e-push-{}-{unique}-{suffix}",
            std::process::id()
        ));
        let state_root = std::env::temp_dir().join(format!(
            "loc-cli-e2e-push-state-{}-{unique}-{suffix}",
            std::process::id()
        ));
        Self {
            root,
            state_root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn content_root(&self) -> PathBuf {
        virtual_fs_content_root(&self.state_root, &self.mount_id)
    }

    fn mount_config(&self) -> MountConfig {
        MountConfig::new(self.mount_id.clone(), "notion", self.root.clone())
            .projection(ProjectionMode::LinuxFuse)
    }

    fn virtual_root_identifier(&self) -> String {
        mount_point_identifier(&self.mount_config())
    }

    fn virtual_root_directory_name(&self) -> String {
        mount_point_directory_name(&self.mount_config())
    }

    fn page_file(&self) -> PathBuf {
        collect_files(&self.root)
            .into_iter()
            .filter(|path| file_name(path) == "page.md")
            .min_by_key(|path| path.components().count())
            .expect("page.md file")
    }

    fn schema_file(&self) -> PathBuf {
        let schemas = collect_files(&self.root)
            .into_iter()
            .filter(|path| file_name(path) == "_schema.yaml")
            .collect::<Vec<_>>();
        schemas
            .iter()
            .find(|path| {
                fs::read_to_string(path)
                    .map(|content| content.contains("\"Related\":"))
                    .unwrap_or(false)
            })
            .cloned()
            .or_else(|| schemas.into_iter().next())
            .expect("database schema file")
    }

    fn database_dir(&self) -> PathBuf {
        self.schema_file()
            .parent()
            .expect("database schema parent")
            .to_path_buf()
    }

    fn nested_markdown_file_containing(&self, needle: &str) -> PathBuf {
        collect_files(&self.root)
            .into_iter()
            .filter(|path| {
                path.extension().is_some_and(|extension| extension == "md")
                    && path.parent().is_some_and(|parent| parent != self.root)
            })
            .find(|path| {
                fs::read_to_string(path)
                    .map(|content| content.contains(needle))
                    .unwrap_or(false)
            })
            .expect("nested markdown file")
    }
}

impl Drop for E2eFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.state_root);
    }
}

#[derive(Clone)]
struct NestedPagePullSource {
    entries: Vec<TreeEntry>,
    child_entries: BTreeMap<PathBuf, Vec<TreeEntry>>,
    rendered: BTreeMap<RemoteId, HydratedEntity>,
}

impl NestedPagePullSource {
    fn new(mount_id: &MountId) -> Self {
        let root_id = "root-page";
        let child_id = "child-page";
        let nested_id = "nested-page";
        let deep_id = "deep-page";
        Self {
            entries: vec![
                nested_pull_tree_entry(mount_id, root_id, "Roadmap", "Roadmap/page.md"),
                nested_pull_tree_entry(
                    mount_id,
                    child_id,
                    "Design Notes",
                    "Roadmap/Design Notes/page.md",
                ),
            ],
            child_entries: BTreeMap::from([
                (
                    PathBuf::from("Roadmap/Design Notes"),
                    vec![nested_pull_tree_entry(
                        mount_id,
                        nested_id,
                        "Appendix",
                        "Roadmap/Design Notes/Appendix/page.md",
                    )],
                ),
                (
                    PathBuf::from("Roadmap/Design Notes/Appendix"),
                    vec![nested_pull_tree_entry(
                        mount_id,
                        deep_id,
                        "Further Reading",
                        "Roadmap/Design Notes/Appendix/Further Reading/page.md",
                    )],
                ),
            ]),
            rendered: BTreeMap::from([
                (
                    RemoteId::new(root_id),
                    nested_pull_hydrated_page(root_id, "Roadmap", "Root body."),
                ),
                (
                    RemoteId::new(child_id),
                    nested_pull_hydrated_page(child_id, "Design Notes", "Design notes body."),
                ),
                (
                    RemoteId::new(nested_id),
                    nested_pull_hydrated_page(nested_id, "Appendix", "Appendix body."),
                ),
                (
                    RemoteId::new(deep_id),
                    nested_pull_hydrated_page(deep_id, "Further Reading", "Further reading body."),
                ),
            ]),
        }
    }
}

impl Connector for NestedPagePullSource {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("nested-page-pull-test")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_lazy_child_enumeration: true,
            ..ConnectorCapabilities::default()
        }
    }

    fn enumerate(
        &self,
        _request: EnumerateRequest,
    ) -> locality_core::LocalityResult<Vec<TreeEntry>> {
        Ok(self.entries.clone())
    }

    fn list_children(
        &self,
        request: ListChildrenRequest,
    ) -> locality_core::LocalityResult<ListChildrenResult> {
        Ok(ListChildrenResult::complete(
            self.child_entries
                .get(&request.parent_path)
                .cloned()
                .unwrap_or_default(),
        ))
    }

    fn fetch(&self, _request: FetchRequest) -> locality_core::LocalityResult<NativeEntity> {
        Err(locality_core::LocalityError::NotImplemented(
            "nested page pull test fetch",
        ))
    }

    fn render(&self, _entity: &NativeEntity) -> locality_core::LocalityResult<CanonicalDocument> {
        Err(locality_core::LocalityError::NotImplemented(
            "nested page pull test render",
        ))
    }

    fn parse(&self, _document: &CanonicalDocument) -> locality_core::LocalityResult<ParsedEntity> {
        Err(locality_core::LocalityError::NotImplemented(
            "nested page pull test parse",
        ))
    }

    fn check_concurrency(
        &self,
        _request: ApplyPlanRequest<'_>,
    ) -> locality_core::LocalityResult<()> {
        Err(locality_core::LocalityError::NotImplemented(
            "nested page pull test concurrency",
        ))
    }

    fn apply(
        &self,
        _request: ApplyPlanRequest<'_>,
    ) -> locality_core::LocalityResult<ApplyPlanResult> {
        Err(locality_core::LocalityError::NotImplemented(
            "nested page pull test apply",
        ))
    }

    fn apply_undo(
        &self,
        _request: ApplyUndoRequest<'_>,
    ) -> locality_core::LocalityResult<ApplyUndoResult> {
        Err(locality_core::LocalityError::NotImplemented(
            "nested page pull test undo",
        ))
    }
}

impl HydrationSource for NestedPagePullSource {
    fn fetch_render(
        &self,
        request: &HydrationRequest,
    ) -> locality_core::LocalityResult<HydratedEntity> {
        assert_eq!(request.reason, HydrationReason::ExplicitPull);
        self.rendered
            .get(&request.remote_id)
            .cloned()
            .ok_or_else(|| {
                locality_core::LocalityError::InvalidState(format!(
                    "missing rendered page {}",
                    request.remote_id.as_str()
                ))
            })
    }
}

impl SourcePushValidator for NestedPagePullSource {}

impl SourceAdapter for NestedPagePullSource {}

fn nested_pull_tree_entry(
    mount_id: &MountId,
    remote_id: &str,
    title: &str,
    path: &str,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(remote_id),
        kind: EntityKind::Page,
        title: title.to_string(),
        path: PathBuf::from(path),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some("2026-06-11T00:00:00.000Z".to_string()),
        stub_frontmatter: None,
    }
}

fn nested_pull_hydrated_page(remote_id: &str, title: &str, body: &str) -> HydratedEntity {
    let document = CanonicalDocument::new(
        format!("loc:\n  id: {remote_id}\n  type: page\ntitle: {title}\n"),
        format!("{body}\n"),
    );
    let body_start_line = document.frontmatter.lines().count() + 3;
    let shadow = ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        document.body.clone(),
        body_start_line,
        [RemoteId::new(format!("{remote_id}-block"))],
    )
    .expect("nested pull shadow")
    .with_frontmatter(document.frontmatter.clone());
    HydratedEntity {
        document,
        shadow,
        remote_edited_at: Some("2026-06-11T00:00:00.000Z".to_string()),
        assets: Vec::new(),
    }
}

fn seed_virtual_page(
    store: &mut InMemoryStateStore,
    fixture: &E2eFixture,
    remote_id: &str,
    title: &str,
    path: &str,
    body: &str,
) {
    seed_virtual_page_for_mount(
        store,
        &fixture.state_root,
        &fixture.mount_id,
        remote_id,
        title,
        path,
        body,
    );
}

fn assert_hydrated_virtual_page(
    store: &InMemoryStateStore,
    fixture: &E2eFixture,
    projection: &ProjectionMode,
    remote_id: &str,
    expected_body: &str,
) {
    let entity = store
        .get_entity(&fixture.mount_id, &RemoteId::new(remote_id))
        .unwrap_or_else(|error| panic!("{projection:?}: get {remote_id}: {error}"))
        .unwrap_or_else(|| panic!("{projection:?}: missing {remote_id} entity"));
    assert_eq!(
        entity.hydration,
        HydrationState::Hydrated,
        "{projection:?}: {entity:#?}"
    );

    let cached_path = fixture.content_root().join(&entity.path);
    let cached = fs::read_to_string(&cached_path).unwrap_or_else(|error| {
        panic!(
            "{projection:?}: read hydrated cache {}: {error}",
            cached_path.display()
        )
    });
    assert!(
        cached.contains(expected_body),
        "{projection:?}: hydrated cache missing {expected_body:?}\n{cached}"
    );
    assert!(
        !cached.contains(CanonicalDocument::STUB_MARKER),
        "{projection:?}: hydrated cache should not remain a stub\n{cached}"
    );

    if matches!(
        projection,
        ProjectionMode::MacosFileProvider | ProjectionMode::WindowsCloudFiles
    ) {
        let visible_path = fixture.root.join(&entity.path);
        let visible = fs::read_to_string(&visible_path).unwrap_or_else(|error| {
            panic!(
                "{projection:?}: read visible provider replica {}: {error}",
                visible_path.display()
            )
        });
        assert!(
            visible.contains(expected_body),
            "{projection:?}: visible provider replica missing {expected_body:?}\n{visible}"
        );
        assert!(
            !visible.contains(CanonicalDocument::STUB_MARKER),
            "{projection:?}: visible provider replica should not remain a stub\n{visible}"
        );
    }
}

fn seed_virtual_page_for_mount(
    store: &mut InMemoryStateStore,
    state_root: &Path,
    mount_id: &MountId,
    remote_id: &str,
    title: &str,
    path: &str,
    body: &str,
) {
    store
        .save_entity(
            EntityRecord::new(
                mount_id.clone(),
                RemoteId::new(remote_id),
                EntityKind::Page,
                title,
                path,
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save virtual page entity");
    let body = format!("{body}\n");
    let frontmatter = format!(
        "loc:\n  id: {remote_id}\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: {title}\n"
    );
    let shadow = ShadowDocument::from_synced_body(
        RemoteId::new(remote_id),
        body.clone(),
        10,
        [RemoteId::new(format!("{remote_id}-block"))],
    )
    .expect("virtual page shadow")
    .with_frontmatter(frontmatter.clone());
    store
        .save_shadow(mount_id, shadow)
        .expect("save virtual page shadow");
    let content_path = virtual_fs_content_root(state_root, mount_id).join(path);
    if let Some(parent) = content_path.parent() {
        fs::create_dir_all(parent).expect("create virtual content parent");
    }
    fs::write(
        content_path,
        render_canonical_markdown(&CanonicalDocument::new(frontmatter, body)),
    )
    .expect("write virtual content page");
}

fn assert_status_issue(
    report: &loc_cli::status::StatusReport,
    path: &str,
    code: &str,
    context: &str,
) {
    let entry = report
        .mounts
        .iter()
        .flat_map(|mount| mount.entries.iter())
        .find(|entry| entry.path == path)
        .unwrap_or_else(|| panic!("{context}: missing status entry for {path}: {report:#?}"));
    assert_eq!(entry.state.as_str(), "dirty", "{context}: {entry:#?}");
    assert_eq!(
        entry.sync_state.as_str(),
        "pending_local_changes",
        "{context}: {entry:#?}"
    );
    assert!(
        entry.issues.iter().any(|issue| issue.code == code),
        "{context}: missing issue {code} for {path}: {entry:#?}"
    );
}

fn assert_error_contains<T: std::fmt::Debug>(
    result: locality_core::LocalityResult<T>,
    expected: &str,
    context: &str,
) {
    let error = result.unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains(expected),
        "{context}: expected error containing `{expected}`, got `{message}`"
    );
}

#[derive(Debug)]
struct StaticScheduledPullSource {
    entries: Vec<TreeEntry>,
    enumeration_count: AtomicU64,
}

impl StaticScheduledPullSource {
    fn new(entries: Vec<TreeEntry>) -> Self {
        Self {
            entries,
            enumeration_count: AtomicU64::new(0),
        }
    }

    fn enumeration_count(&self) -> u64 {
        self.enumeration_count.load(Ordering::SeqCst)
    }
}

impl ScheduledPullSource for StaticScheduledPullSource {
    fn enumerate_mount(
        &self,
        mount: &MountConfig,
    ) -> locality_core::LocalityResult<Vec<TreeEntry>> {
        self.enumeration_count.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .entries
            .iter()
            .filter(|entry| entry.mount_id == mount.mount_id)
            .cloned()
            .collect())
    }
}

#[derive(Debug)]
struct CountingScheduledPullSource<'a, S: ?Sized> {
    inner: &'a S,
    enumeration_count: AtomicU64,
    schema_count: AtomicU64,
}

impl<'a, S: ?Sized> CountingScheduledPullSource<'a, S> {
    fn new(inner: &'a S) -> Self {
        Self {
            inner,
            enumeration_count: AtomicU64::new(0),
            schema_count: AtomicU64::new(0),
        }
    }

    fn enumeration_count(&self) -> u64 {
        self.enumeration_count.load(Ordering::SeqCst)
    }

    fn schema_count(&self) -> u64 {
        self.schema_count.load(Ordering::SeqCst)
    }
}

impl<S> ScheduledPullSource for CountingScheduledPullSource<'_, S>
where
    S: ScheduledPullSource + ?Sized,
{
    fn enumerate_mount(
        &self,
        mount: &MountConfig,
    ) -> locality_core::LocalityResult<Vec<TreeEntry>> {
        self.enumeration_count.fetch_add(1, Ordering::SeqCst);
        self.inner.enumerate_mount(mount)
    }

    fn database_schema_yaml(
        &self,
        mount: &MountConfig,
        remote_id: &RemoteId,
    ) -> locality_core::LocalityResult<Option<String>> {
        self.schema_count.fetch_add(1, Ordering::SeqCst);
        self.inner.database_schema_yaml(mount, remote_id)
    }
}

#[derive(Clone, Default)]
struct WallClockScheduledPullRunner {
    scheduled_count: Arc<AtomicUsize>,
}

impl WallClockScheduledPullRunner {
    fn scheduled_count(&self) -> usize {
        self.scheduled_count.load(Ordering::SeqCst)
    }

    fn wait_for_scheduled_count(&self, expected: usize, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let observed = self.scheduled_count();
            if observed >= expected || Instant::now() >= deadline {
                return observed;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl localityd::runtime::RuntimeJobRunner for WallClockScheduledPullRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> localityd::ipc::DaemonResponse {
        localityd::ipc::DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> localityd::ipc::DaemonResponse {
        localityd::ipc::DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<localityd::runtime::ScheduledPullRuntimeReport> {
        self.scheduled_count.fetch_add(1, Ordering::SeqCst);
        Ok(localityd::runtime::ScheduledPullRuntimeReport {
            report: Default::default(),
            queued_hydrations: Vec::new(),
            freshness_jobs: Vec::new(),
        })
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Err(locality_core::LocalityError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }
}

#[derive(Clone)]
struct LiveDaemonScheduledPullRunner {
    connector: NotionConnector,
    scheduled_count: Arc<AtomicUsize>,
    api_enumeration_count: Arc<AtomicUsize>,
    reports: Arc<Mutex<Vec<LiveDaemonScheduledPullSummary>>>,
    last_error: Arc<Mutex<Option<String>>>,
}

#[derive(Clone, Debug)]
struct LiveDaemonScheduledPullSummary {
    mounts_polled: usize,
    enumerated: usize,
    queued_hydrations: usize,
}

impl LiveDaemonScheduledPullRunner {
    fn new(connector: NotionConnector) -> Self {
        Self {
            connector,
            scheduled_count: Arc::new(AtomicUsize::new(0)),
            api_enumeration_count: Arc::new(AtomicUsize::new(0)),
            reports: Arc::new(Mutex::new(Vec::new())),
            last_error: Arc::new(Mutex::new(None)),
        }
    }

    fn wait_for_scheduled_count(&self, expected: usize, timeout: Duration) -> usize {
        let deadline = Instant::now() + timeout;
        loop {
            let observed = self.scheduled_count.load(Ordering::SeqCst);
            if observed >= expected || Instant::now() >= deadline {
                return observed;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn api_enumeration_count(&self) -> usize {
        self.api_enumeration_count.load(Ordering::SeqCst)
    }

    fn reports(&self) -> Vec<LiveDaemonScheduledPullSummary> {
        self.reports
            .lock()
            .expect("live daemon scheduled reports")
            .clone()
    }

    fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .expect("live daemon scheduled error")
            .clone()
    }
}

impl localityd::runtime::RuntimeJobRunner for LiveDaemonScheduledPullRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> localityd::ipc::DaemonResponse {
        localityd::ipc::DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> localityd::ipc::DaemonResponse {
        localityd::ipc::DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        state_root: PathBuf,
        tick: PullSchedulerTick,
        policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<localityd::runtime::ScheduledPullRuntimeReport> {
        let result: locality_core::LocalityResult<localityd::runtime::ScheduledPullRuntimeReport> =
            (|| {
                let mut store = SqliteStateStore::open(state_root.clone())
                    .map_err(locality_core::LocalityError::from)?;
                let mounts = store
                    .load_mounts()
                    .map_err(locality_core::LocalityError::from)?;
                let mut hydration = HydrationQueue::new();
                let report = localityd::reconcile::reconcile_scheduled_pull_with_state_root(
                    &mut store,
                    &mut hydration,
                    &mounts,
                    &tick,
                    &self.connector,
                    &DefaultFetchScheduleStrategy,
                    &policy,
                    Some(&state_root),
                )?;
                let mut queued_hydrations = Vec::new();
                while let Some(request) = hydration.pop_ready() {
                    queued_hydrations.push(request);
                }

                self.api_enumeration_count
                    .fetch_add(report.mounts_polled, Ordering::SeqCst);
                self.reports
                    .lock()
                    .expect("record live daemon scheduled report")
                    .push(LiveDaemonScheduledPullSummary {
                        mounts_polled: report.mounts_polled,
                        enumerated: report.enumerated,
                        queued_hydrations: report.queued_hydrations,
                    });
                self.scheduled_count.fetch_add(1, Ordering::SeqCst);

                Ok(localityd::runtime::ScheduledPullRuntimeReport {
                    report,
                    queued_hydrations,
                    freshness_jobs: Vec::new(),
                })
            })();

        if let Err(error) = &result {
            *self.last_error.lock().expect("record live daemon error") = Some(error.to_string());
        }

        result
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        Ok(HydrationOutcome::Hydrated)
    }
}

fn save_workspace_freshness_page(
    store: &mut InMemoryStateStore,
    mount_id: &MountId,
    remote_id: &str,
    title: impl Into<String>,
    path: impl Into<PathBuf>,
    hydration: HydrationState,
) {
    store
        .save_entity(
            EntityRecord::new(
                mount_id.clone(),
                RemoteId::new(remote_id),
                EntityKind::Page,
                title,
                path,
            )
            .with_hydration(hydration),
        )
        .expect("save workspace freshness entity");
}

fn scheduled_tree_entries(mount_id: &MountId, child_count: usize) -> Vec<TreeEntry> {
    let mut entries = Vec::with_capacity(child_count + 1);
    entries.push(scheduled_page_entry(
        mount_id,
        "root-page",
        "Root",
        "Root/page.md",
        "2026-06-10T00:00:00.000Z",
    ));
    for number in 1..=child_count {
        entries.push(scheduled_page_entry(
            mount_id,
            &format!("child-{number}"),
            &format!("Child {number}"),
            format!("Root/Child {number}/page.md"),
            &format!("2026-06-10T00:00:{number:02}.000Z"),
        ));
    }
    entries
}

fn scheduled_page_entry(
    mount_id: &MountId,
    remote_id: &str,
    title: &str,
    path: impl Into<PathBuf>,
    remote_edited_at: &str,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(remote_id),
        kind: EntityKind::Page,
        title: title.to_string(),
        path: path.into(),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(remote_edited_at.to_string()),
        stub_frontmatter: None,
    }
}

fn mount_virtual_workspace(fixture: &E2eFixture, store: &mut InMemoryStateStore, root_id: &str) {
    mount_virtual_workspace_with_projection(
        fixture,
        store,
        root_id,
        fixture.mount_config().projection,
    );
}

fn mount_virtual_workspace_with_projection(
    fixture: &E2eFixture,
    store: &mut InMemoryStateStore,
    root_id: &str,
    projection: ProjectionMode,
) {
    let mount = fixture.mount_config();
    run_mount(
        store,
        MountOptions {
            mount_id: mount.mount_id,
            connector: mount.connector,
            root: mount.root,
            remote_root_id: Some(RemoteId::new(root_id.to_string())),
            connection_id: None,
            read_only: false,
            projection,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount virtual live workspace");
}

fn hydrate_virtual_root_page(
    fixture: &E2eFixture,
    store: &mut InMemoryStateStore,
    connector: &NotionConnector,
    content_root: &Path,
    page_id: &str,
) {
    let mount_point_root = fixture.virtual_root_identifier();
    refresh_virtual_fs_children(store, connector, &fixture.mount_id, &mount_point_root)
        .expect("refresh virtual mount point root");
    materialize_virtual_fs_item_with_content_root(
        store,
        connector,
        content_root,
        &fixture.mount_id,
        page_id,
    )
    .expect("hydrate virtual root page");
}

fn find_virtual_folder<'a>(
    items: &'a [localityd::virtual_fs::VirtualFsItem],
    remote_id: &str,
) -> &'a localityd::virtual_fs::VirtualFsItem {
    items
        .iter()
        .find(|item| {
            item.remote_id.as_deref() == Some(remote_id)
                && item.kind == localityd::virtual_fs::VirtualFsItemKind::Folder
        })
        .unwrap_or_else(|| panic!("missing virtual folder for {remote_id}: {items:#?}"))
}

fn refresh_virtual_children_until_remote_folder(
    store: &mut InMemoryStateStore,
    connector: &NotionConnector,
    content_root: &Path,
    mount_id: &MountId,
    container_identifier: &str,
    remote_id: &str,
) -> localityd::virtual_fs::VirtualFsItem {
    let mut last_children = None;
    for _ in 0..8 {
        refresh_virtual_fs_children(store, connector, mount_id, container_identifier)
            .expect("refresh virtual children");
        let report = virtual_fs_children_with_content_root(
            store,
            content_root,
            mount_id,
            container_identifier,
        )
        .expect("list virtual children after refresh");
        if let Some(child) = report
            .children
            .iter()
            .find(|item| {
                item.remote_id.as_deref() == Some(remote_id)
                    && item.kind == localityd::virtual_fs::VirtualFsItemKind::Folder
            })
            .cloned()
        {
            return child;
        }
        last_children = Some(report.children);
        std::thread::sleep(Duration::from_millis(500));
    }

    panic!(
        "missing virtual folder for {remote_id} after refreshed parent `{container_identifier}`: {:#?}",
        last_children.unwrap_or_default()
    );
}

fn desktop_style_locate_notion_url_path(
    store: &mut InMemoryStateStore,
    connector: &NotionConnector,
    mount_id: &MountId,
    url: &str,
) -> PathBuf {
    let notion_id = notion_id_from_url(url).expect("Notion URL id");
    let remote_id = RemoteId::new(notion_id.clone());
    let mut last_error = "unknown error".to_string();
    let mut resolved_entries = None;
    for _ in 0..8 {
        match connector.resolve_object_path_entries(mount_id.clone(), &remote_id) {
            Ok(entries) => {
                resolved_entries = Some(entries);
                break;
            }
            Err(error) => {
                last_error = error.to_string();
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    let entries = resolved_entries
        .unwrap_or_else(|| panic!("resolve exact Notion URL path entries: {last_error}"));
    assert!(
        entries
            .iter()
            .any(|entry| compact_notion_id(entry.remote_id.as_str()) == notion_id),
        "exact Notion URL resolver did not return target {remote_id:?}: {entries:#?}"
    );
    for entry in entries {
        store
            .save_entity(EntityRecord::from(entry))
            .expect("save exact located Notion metadata");
    }

    let search = run_search(
        store,
        SearchOptions {
            query: url.to_string(),
            connector: Some("notion".to_string()),
            limit: 8,
            include_stale_access: false,
        },
    )
    .expect("search exact located Notion URL");
    let located = search
        .results
        .iter()
        .find(|result| compact_notion_id(&result.remote_id) == notion_id)
        .unwrap_or_else(|| panic!("missing exact located Notion result: {search:#?}"));
    assert_eq!(located.kind, "page", "{located:#?}");
    assert!(
        located.path.ends_with("/page.md"),
        "desktop locate should resolve a page URL to page.md: {located:#?}"
    );
    PathBuf::from(&located.absolute_path)
}

#[derive(Debug)]
struct LiveCleanup {
    api: HttpNotionApi,
    block_ids: Vec<String>,
}

impl LiveCleanup {
    fn new(api: HttpNotionApi) -> Self {
        Self {
            api,
            block_ids: Vec::new(),
        }
    }

    fn create_page(&mut self, parent_page_id: &str, title: &str, children: Vec<Value>) -> PageDto {
        let mut body = json!({
            "parent": {
                "type": "page_id",
                "page_id": parent_page_id,
            },
            "properties": {
                "title": {
                    "title": [
                        {
                            "type": "text",
                            "text": {
                                "content": title,
                            }
                        }
                    ]
                },
            },
        });
        if !children.is_empty() {
            body["children"] = Value::Array(children);
        }
        let page = self
            .api
            .create_page(body)
            .expect("create live scratch page");
        self.block_ids.push(page.id.clone());
        page
    }

    fn current_user_id(&self) -> String {
        self.api
            .retrieve_current_user()
            .expect("retrieve current Notion user")
            .get("id")
            .and_then(Value::as_str)
            .expect("current Notion user id")
            .to_string()
    }

    fn create_database(&mut self, parent_page_id: &str, title: &str) -> DatabaseDto {
        self.create_database_with_optional_relation(parent_page_id, title, None)
    }

    fn create_database_with_relation(
        &mut self,
        parent_page_id: &str,
        title: &str,
        related_data_source_id: &str,
    ) -> DatabaseDto {
        self.create_database_with_optional_relation(
            parent_page_id,
            title,
            Some(related_data_source_id),
        )
    }

    fn create_database_with_optional_relation(
        &mut self,
        parent_page_id: &str,
        title: &str,
        related_data_source_id: Option<&str>,
    ) -> DatabaseDto {
        let unique_prefix = unique_id_prefix();
        let mut properties = serde_json::Map::from_iter([
            ("Name".to_string(), json!({ "title": {} })),
            ("Notes".to_string(), json!({ "rich_text": {} })),
            (
                "Points".to_string(),
                json!({ "number": { "format": "number" } }),
            ),
            (
                "Status".to_string(),
                json!({
                    "select": {
                        "options": [
                            { "name": "Todo", "color": "gray" },
                            { "name": "Done", "color": "green" }
                        ]
                    }
                }),
            ),
            ("State".to_string(), json!({ "status": {} })),
            (
                "Tags".to_string(),
                json!({
                    "multi_select": {
                        "options": [
                            { "name": "Alpha", "color": "blue" },
                            { "name": "Beta", "color": "purple" }
                        ]
                    }
                }),
            ),
            ("Done".to_string(), json!({ "checkbox": {} })),
            ("Due".to_string(), json!({ "date": {} })),
            ("URL".to_string(), json!({ "url": {} })),
            ("Email".to_string(), json!({ "email": {} })),
            ("Phone".to_string(), json!({ "phone_number": {} })),
            ("Files".to_string(), json!({ "files": {} })),
            ("People".to_string(), json!({ "people": {} })),
            (
                "Unique".to_string(),
                json!({ "unique_id": { "prefix": unique_prefix } }),
            ),
        ]);
        if let Some(data_source_id) = related_data_source_id {
            properties.insert(
                "Related".to_string(),
                json!({
                    "relation": {
                        "data_source_id": data_source_id,
                        "single_property": {},
                    }
                }),
            );
        }
        let database = self
            .api
            .create_database(json!({
                "parent": {
                    "type": "page_id",
                    "page_id": parent_page_id,
                },
                "title": rich_text_json(title),
                "initial_data_source": {
                    "title": rich_text_json("Rows"),
                    "properties": Value::Object(properties)
                }
            }))
            .expect("create live database");
        self.block_ids.push(database.id.clone());
        database
    }

    fn create_database_row(
        &mut self,
        database: &DatabaseDto,
        title: &str,
        mut properties: serde_json::Map<String, Value>,
        children: Vec<Value>,
    ) -> PageDto {
        let data_source = database
            .data_sources
            .first()
            .expect("created database data source");
        properties.insert(
            "Name".to_string(),
            json!({ "title": rich_text_json(title) }),
        );
        let mut body = json!({
            "parent": {
                "type": "data_source_id",
                "data_source_id": data_source.id,
            },
            "properties": Value::Object(properties),
        });
        if !children.is_empty() {
            body["children"] = Value::Array(children);
        }
        let page = self
            .api
            .create_page(body)
            .expect("create live database row");
        self.block_ids.push(page.id.clone());
        page
    }
}

impl Drop for LiveCleanup {
    fn drop(&mut self) {
        for block_id in self.block_ids.iter().rev() {
            let _ = self.api.delete_block(block_id);
        }
    }
}

fn live_notion_config() -> NotionConfig {
    NotionConfig::default().with_token(live_notion_token())
}

fn live_notion_token() -> String {
    if let Ok(token) = std::env::var(TOKEN_ENV) {
        let token = token.trim();
        if !token.is_empty() {
            return token.to_string();
        }
    }

    let state_root = locality_platform::default_state_root();
    let connection_id =
        std::env::var(LIVE_CONNECTION_ENV).unwrap_or_else(|_| "notion-default".to_string());
    live_notion_token_from_state_root(&state_root, &connection_id).unwrap_or_else(|error| {
        panic!(
            "set {TOKEN_ENV} or store a Notion credential for `{connection_id}` in `{}` ({error})",
            state_root.join("credentials").display()
        )
    })
}

fn live_notion_token_from_state_root(
    state_root: &Path,
    connection_id: &str,
) -> Result<String, String> {
    notion_access_token_from_secret(&live_notion_secret_from_state_root(
        state_root,
        connection_id,
    )?)
}

fn live_notion_secret_from_default_store(connection_id: &str) -> Result<String, String> {
    live_notion_secret_from_state_root(&locality_platform::default_state_root(), connection_id)
}

fn live_notion_secret_from_state_root(
    state_root: &Path,
    connection_id: &str,
) -> Result<String, String> {
    let secret_ref = format!("connection:{connection_id}");
    let credentials = open_credential_store(state_root);
    credentials
        .get(&secret_ref)
        .map_err(|error| format!("failed to read `{secret_ref}`: {error}"))
}

fn notion_access_token_from_secret(secret: &str) -> Result<String, String> {
    let secret = secret.trim();
    if secret.is_empty() {
        return Err("credential secret is empty".to_string());
    }
    if secret.starts_with('{') {
        let stored = serde_json::from_str::<StoredNotionCredential>(secret)
            .map_err(|error| format!("stored Notion OAuth credential is invalid: {error}"))?;
        if stored.access_token.trim().is_empty() {
            return Err("stored Notion OAuth credential has an empty access token".to_string());
        }
        return Ok(stored.access_token);
    }
    Ok(secret.to_string())
}

fn seed_cli_live_connection(state_root: &Path, connection_id: &ConnectionId, stored_secret: &str) {
    fs::create_dir_all(state_root).expect("create CLI live state root");
    let secret_ref = format!("connection:{}", connection_id.as_str());
    let auth_kind = if stored_secret.trim_start().starts_with('{') {
        "oauth"
    } else {
        "token"
    };
    let profile_id = ConnectorProfileId::new(if auth_kind == "oauth" {
        DEFAULT_NOTION_OAUTH_PROFILE_ID
    } else {
        DEFAULT_NOTION_PROFILE_ID
    });
    let credentials = FileCredentialStore::new(state_root);
    credentials
        .put(&secret_ref, stored_secret)
        .expect("seed CLI live credential");

    let stored_oauth = if auth_kind == "oauth" {
        Some(
            serde_json::from_str::<StoredNotionCredential>(stored_secret)
                .expect("stored Notion OAuth credential"),
        )
    } else {
        None
    };
    let now = timestamp_string();
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open CLI live state");
    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: "notion".to_string(),
            display_name: if auth_kind == "oauth" {
                "Notion OAuth".to_string()
            } else {
                "Notion token auth".to_string()
            },
            auth_kind: auth_kind.to_string(),
            scopes: vec![],
            capabilities_json: notion_capabilities_json_for_live_test(),
            enabled_actions_json: "[\"read\",\"write\"]".to_string(),
            connector_version: "notion.v1".to_string(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
        })
        .expect("seed CLI live connector profile");
    store
        .save_connection(ConnectionRecord {
            connection_id: connection_id.clone(),
            profile_id: Some(profile_id),
            connector: "notion".to_string(),
            display_name: connection_id.as_str().to_string(),
            account_label: stored_oauth
                .as_ref()
                .and_then(|stored| stored.workspace_name.clone()),
            workspace_id: stored_oauth
                .as_ref()
                .and_then(|stored| stored.workspace_id.clone()),
            workspace_name: stored_oauth
                .as_ref()
                .and_then(|stored| stored.workspace_name.clone()),
            auth_kind: auth_kind.to_string(),
            secret_ref,
            scopes: vec![],
            capabilities_json: notion_capabilities_json_for_live_test(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            expires_at: None,
        })
        .expect("seed CLI live connection");
}

fn notion_capabilities_json_for_live_test() -> String {
    serde_json::to_string(&ConnectorCapabilities {
        supports_block_updates: true,
        supports_databases: true,
        supports_oauth: true,
        supports_remote_observation: true,
        supports_lazy_child_enumeration: true,
        supports_media_download: true,
        supports_undo: true,
        supports_batch_observation: false,
    })
    .expect("serialize Notion capabilities")
}

#[derive(Debug)]
struct LocJsonOutput {
    value: Value,
    stdout: String,
}

fn loc_command(loc: &str, state_root: &Path) -> Command {
    let mut command = Command::new(loc);
    command
        .env("LOCALITY_STATE_DIR", state_root)
        .env("LOCALITY_DAEMON_DISABLE", "1")
        .env_remove(TOKEN_ENV);
    command
}

fn set_directory_readonly(path: &Path, readonly: bool) {
    let mut permissions = fs::metadata(path)
        .unwrap_or_else(|error| panic!("read permissions for {}: {error}", path.display()))
        .permissions();
    permissions.set_readonly(readonly);
    fs::set_permissions(path, permissions)
        .unwrap_or_else(|error| panic!("set permissions for {}: {error}", path.display()));
}

fn loc_json_ok(command: &mut Command) -> LocJsonOutput {
    loc_json_with_exit(command, 0)
}

fn loc_json_with_exit(command: &mut Command, expected_code: i32) -> LocJsonOutput {
    let output = command.output().expect("run loc command");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output.status.code().unwrap_or(-1);
    assert!(
        code == expected_code,
        "loc command exited with {code}, expected {expected_code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!("failed to parse loc JSON: {error}\nstdout:\n{stdout}\nstderr:\n{stderr}")
    });
    LocJsonOutput { value, stdout }
}

fn loc_text_with_exit(command: &mut Command, expected_code: i32) -> String {
    let output = command.output().expect("run loc command");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output.status.code().unwrap_or(-1);
    assert!(
        code == expected_code,
        "loc command exited with {code}, expected {expected_code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.trim().is_empty(),
        "loc command should not print stderr on success\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
}

#[derive(Debug)]
struct LiveEnv {
    parent_page_id: String,
}

impl LiveEnv {
    fn from_env() -> Self {
        let parent_page = std::env::var(LIVE_PARENT_ENV)
            .unwrap_or_else(|_| panic!("set {LIVE_PARENT_ENV} to a writable page ID or URL"));
        let parent_page_id = normalize_notion_id(&parent_page);
        let api = HttpNotionApi::new(live_notion_config());
        let parent = api.retrieve_page(&parent_page_id).unwrap_or_else(|error| {
            panic!(
                "{LIVE_PARENT_ENV} `{parent_page_id}` must point to an accessible writable Notion page; failed to retrieve parent page: {error}"
            )
        });
        ensure_live_parent_page_is_writable(&parent, &parent_page_id);
        Self { parent_page_id }
    }
}

fn ensure_live_parent_page_is_writable(parent: &PageDto, parent_page_id: &str) {
    if parent.archived || parent.in_trash {
        panic!(
            "{LIVE_PARENT_ENV} `{parent_page_id}` points to a Notion page that is archived or in trash; choose an active writable parent page"
        );
    }
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return message.to_string();
    }
    "<non-string panic payload>".to_string()
}

fn pull_live_page(
    connector: &NotionConnector,
    page_id: &str,
) -> (E2eFixture, InMemoryStateStore, PathBuf, String) {
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();

    run_mount(
        &mut store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(page_id.to_string())),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live page");
    run_pull(&mut store, connector, &fixture.root).expect("pull live page");
    let page_path = fixture.page_file();
    let markdown = fs::read_to_string(&page_path).expect("read live page markdown");
    (fixture, store, page_path, markdown)
}

fn live_block_snapshot(connector: &NotionConnector, page_id: &str) -> Value {
    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new(page_id.to_string()),
        })
        .expect("fetch live snapshot");
    let bundle: NotionPageBundle = serde_json::from_slice(&native.raw).expect("snapshot bundle");
    serde_json::to_value(bundle.blocks).expect("snapshot json")
}

fn live_page_snapshot(connector: &NotionConnector, page_id: &str) -> Value {
    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new(page_id.to_string()),
        })
        .expect("fetch live page snapshot");
    let bundle: NotionPageBundle = serde_json::from_slice(&native.raw).expect("snapshot bundle");
    serde_json::to_value(bundle).expect("snapshot json")
}

fn render_live_page(connector: &NotionConnector, page_id: &str, page_path: &Path) -> String {
    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new(page_id.to_string()),
        })
        .expect("fetch live page");
    connector
        .render_native_entity_for_path(&native, page_path)
        .expect("render live page")
        .document
        .body
}

fn render_live_markdown(connector: &NotionConnector, page_id: &str, page_path: &Path) -> String {
    let native = connector
        .fetch(FetchRequest {
            remote_id: RemoteId::new(page_id.to_string()),
        })
        .expect("fetch live page");
    let document = connector
        .render_native_entity_for_path(&native, page_path)
        .expect("render live page")
        .document;
    render_canonical_markdown(&document)
}

fn append_remote_paragraph(api: &HttpNotionApi, page_id: &str, text: &str) {
    api.append_block_children(
        page_id,
        json!({
            "children": [paragraph_child(text)]
        }),
    )
    .expect("append live remote paragraph");
}

fn append_remote_paragraph_and_wait(api: &HttpNotionApi, page_id: &str, text: &str, context: &str) {
    append_remote_paragraph(api, page_id, text);
    wait_for_live_block_text(api, page_id, text, context);
}

fn first_live_child_block_id(api: &HttpNotionApi, page_id: &str) -> String {
    api.retrieve_block_children(page_id, None)
        .expect("retrieve live child blocks")
        .results
        .first()
        .expect("live page child block")
        .id
        .clone()
}

fn update_remote_paragraph(api: &HttpNotionApi, block_id: &str, text: &str) {
    api.update_block(
        block_id,
        json!({
            "paragraph": {
                "rich_text": rich_text_json(text)
            }
        }),
    )
    .expect("update live remote paragraph");
}

fn update_remote_paragraph_and_wait(
    api: &HttpNotionApi,
    page_id: &str,
    block_id: &str,
    text: &str,
    context: &str,
) {
    update_remote_paragraph(api, block_id, text);
    wait_for_live_block_text(api, page_id, text, context);
}

fn wait_for_live_block_text(api: &HttpNotionApi, page_id: &str, text: &str, context: &str) {
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut last_snapshot = String::new();

    while Instant::now() < deadline {
        let children = api
            .retrieve_block_children(page_id, None)
            .unwrap_or_else(|error| panic!("{context}: retrieve live child blocks: {error}"));
        last_snapshot =
            serde_json::to_string(&children.results).expect("serialize live block snapshot");
        if last_snapshot.contains(text) {
            return;
        }
        thread::sleep(Duration::from_millis(250));
    }

    panic!(
        "{context}: live page `{page_id}` did not expose expected text `{text}` before timeout; last observed blocks: {last_snapshot}"
    );
}

fn wait_for_live_deleted_observation(
    connector: &NotionConnector,
    mount_id: &MountId,
    page_id: &str,
    context: &str,
) -> RemoteObservation {
    let deadline = Instant::now() + Duration::from_secs(12);
    let remote_id = RemoteId::new(page_id.to_string());
    let mut last_observation = String::new();

    while Instant::now() < deadline {
        match connector.observe(ObserveRequest {
            mount_id: mount_id.clone(),
            remote_id: remote_id.clone(),
        }) {
            Ok(observation) if observation.deleted => return observation,
            Ok(observation) => {
                last_observation = format!("{observation:#?}");
            }
            Err(error) => {
                last_observation = error.to_string();
            }
        }
        thread::sleep(Duration::from_millis(250));
    }

    panic!(
        "{context}: live page `{page_id}` did not expose a deleted observation before timeout; last observation: {last_observation}"
    );
}

fn diverse_page_children(target_page_id: &str, database_id: &str) -> Vec<Value> {
    vec![
        json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": [
                    text_part("Cyclic paragraph with "),
                    annotated_text("bold", "bold"),
                    text_part(" and a target mention "),
                    page_mention_part("Target page", target_page_id),
                    text_part(" and database mention "),
                    database_mention_part("Linked database", database_id),
                    text_part(" plus inline math "),
                    equation_part("a^2+b^2=c^2")
                ]
            }
        }),
        rich_text_child("heading_1", "Cyclic heading one"),
        rich_text_child("heading_2", "Cyclic heading two"),
        rich_text_child("heading_3", "Cyclic heading three"),
        rich_text_child("heading_4", "Cyclic heading four"),
        rich_text_child("bulleted_list_item", "Cyclic bullet"),
        rich_text_child("numbered_list_item", "Cyclic number"),
        json!({
            "object": "block",
            "type": "to_do",
            "to_do": { "rich_text": rich_text_json("Cyclic todo"), "checked": false }
        }),
        rich_text_child("quote", "Cyclic quote"),
        rich_text_child("callout", "Cyclic callout"),
        json!({
            "object": "block",
            "type": "toggle",
            "toggle": {
                "rich_text": rich_text_json("Cyclic toggle"),
                "children": [paragraph_child("Cyclic toggle child")]
            }
        }),
        json!({
            "object": "block",
            "type": "code",
            "code": { "rich_text": rich_text_json("fn cyclic() {}"), "language": "rust" }
        }),
        json!({ "object": "block", "type": "divider", "divider": {} }),
        json!({
            "object": "block",
            "type": "equation",
            "equation": { "expression": "a^2+b^2=c^2" }
        }),
        json!({
            "object": "block",
            "type": "bookmark",
            "bookmark": { "url": "https://example.com/cyclic-bookmark", "caption": rich_text_json("Cyclic bookmark") }
        }),
        json!({
            "object": "block",
            "type": "embed",
            "embed": { "url": "https://example.com/cyclic-embed", "caption": rich_text_json("Cyclic embed") }
        }),
        json!({
            "object": "block",
            "type": "table",
            "table": {
                "table_width": 2,
                "has_column_header": true,
                "has_row_header": false,
                "children": [
                    table_row_child("Left", "Right"),
                    table_row_child("Cell A", "Cell B")
                ]
            }
        }),
        json!({
            "object": "block",
            "type": "column_list",
            "column_list": {
                "children": [
                    { "object": "block", "type": "column", "column": { "children": [paragraph_child("Cyclic column one")] } },
                    { "object": "block", "type": "column", "column": { "children": [paragraph_child("Cyclic column two")] } }
                ]
            }
        }),
        json!({
            "object": "block",
            "type": "table_of_contents",
            "table_of_contents": { "color": "default" }
        }),
        json!({ "object": "block", "type": "breadcrumb", "breadcrumb": {} }),
        json!({
            "object": "block",
            "type": "link_to_page",
            "link_to_page": { "type": "page_id", "page_id": target_page_id }
        }),
        json!({
            "object": "block",
            "type": "link_to_page",
            "link_to_page": { "type": "database_id", "database_id": database_id }
        }),
        media_child(
            "image",
            "https://www.w3.org/Icons/w3c_home.png",
            "Cyclic image",
        ),
        media_child(
            "video",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "Cyclic video",
        ),
        media_child(
            "file",
            "https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf",
            "Cyclic file",
        ),
        media_child(
            "pdf",
            "https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf",
            "Cyclic PDF",
        ),
        media_child(
            "audio",
            "https://www.soundhelix.com/examples/mp3/SoundHelix-Song-1.mp3",
            "Cyclic audio",
        ),
    ]
}

fn supported_edit_children(
    user_id: &str,
    target_page_id: &str,
    linked_database_id: &str,
) -> Vec<Value> {
    vec![
        paragraph_child("Editable paragraph original."),
        json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": [text_part("Editable date "), date_mention_part("2026-06-13")]
            }
        }),
        json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": [text_part("Editable user "), user_mention_part(user_id)]
            }
        }),
        json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": [
                    text_part("Editable typed links "),
                    page_mention_part("Target page", target_page_id),
                    text_part(" and "),
                    database_mention_part("Linked database", linked_database_id),
                ]
            }
        }),
        rich_text_child("heading_1", "Editable heading one"),
        rich_text_child("heading_2", "Editable heading two"),
        rich_text_child("heading_3", "Editable heading three"),
        rich_text_child("heading_4", "Editable heading four"),
        rich_text_child("bulleted_list_item", "Editable bullet"),
        rich_text_child("numbered_list_item", "Editable number"),
        json!({
            "object": "block",
            "type": "to_do",
            "to_do": { "rich_text": rich_text_json("Editable todo"), "checked": false }
        }),
        rich_text_child("quote", "Editable quote"),
        rich_text_child("callout", "Editable callout"),
        json!({
            "object": "block",
            "type": "bookmark",
            "bookmark": { "url": "https://example.com/editable-bookmark", "caption": rich_text_json("Editable bookmark") }
        }),
        json!({
            "object": "block",
            "type": "embed",
            "embed": { "url": "https://example.com/editable-embed", "caption": rich_text_json("Editable embed") }
        }),
        json!({
            "object": "block",
            "type": "table",
            "table": {
                "table_width": 2,
                "has_column_header": true,
                "has_row_header": false,
                "children": [
                    table_row_child("Editable table name", "Editable table status"),
                    table_row_child("Editable table item", "Editable table state")
                ]
            }
        }),
        media_child(
            "image",
            "https://www.w3.org/Icons/w3c_home.png",
            "Editable image",
        ),
        media_child(
            "video",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "Editable video",
        ),
        media_child(
            "file",
            "https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf",
            "Editable file",
        ),
        media_child(
            "pdf",
            "https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf",
            "Editable PDF",
        ),
        media_child(
            "audio",
            "https://www.soundhelix.com/examples/mp3/SoundHelix-Song-1.mp3",
            "Editable audio",
        ),
        json!({
            "object": "block",
            "type": "code",
            "code": { "rich_text": rich_text_json("fn editable() {}"), "language": "rust" }
        }),
        json!({ "object": "block", "type": "divider", "divider": {} }),
        json!({
            "object": "block",
            "type": "equation",
            "equation": { "expression": "x+y=z" }
        }),
    ]
}

fn paragraph_child(text: &str) -> Value {
    rich_text_child("paragraph", text)
}

fn rich_text_child(kind: &str, text: &str) -> Value {
    let mut block = json!({
        "object": "block",
        "type": kind
    });
    block[kind] = json!({ "rich_text": rich_text_json(text) });
    block
}

fn table_row_child(left: &str, right: &str) -> Value {
    json!({
        "object": "block",
        "type": "table_row",
        "table_row": {
            "cells": [rich_text_json(left), rich_text_json(right)]
        }
    })
}

fn media_child(kind: &str, url: &str, caption: &str) -> Value {
    let mut block = json!({
        "object": "block",
        "type": kind
    });
    block[kind] = json!({
        "type": "external",
        "external": { "url": url },
        "caption": rich_text_json(caption)
    });
    block
}

fn rich_text_json(text: &str) -> Vec<Value> {
    vec![text_part(text)]
}

fn text_part(text: &str) -> Value {
    json!({
        "type": "text",
        "text": { "content": text }
    })
}

fn linked_text_part(text: &str, url: &str) -> Value {
    json!({
        "type": "text",
        "text": {
            "content": text,
            "link": { "url": url }
        },
        "href": url,
        "plain_text": text
    })
}

fn annotated_text(text: &str, annotation: &str) -> Value {
    let mut annotations = serde_json::Map::new();
    annotations.insert(annotation.to_string(), json!(true));
    json!({
        "type": "text",
        "text": { "content": text },
        "annotations": Value::Object(annotations)
    })
}

fn colored_text_part(text: &str, color: &str) -> Value {
    json!({
        "type": "text",
        "text": { "content": text },
        "annotations": {
            "bold": false,
            "italic": false,
            "strikethrough": false,
            "underline": false,
            "code": false,
            "color": color
        }
    })
}

fn equation_part(expression: &str) -> Value {
    json!({
        "type": "equation",
        "equation": { "expression": expression }
    })
}

fn page_mention_part(label: &str, page_id: &str) -> Value {
    json!({
        "type": "mention",
        "mention": {
            "type": "page",
            "page": { "id": page_id }
        },
        "plain_text": label
    })
}

fn database_mention_part(label: &str, database_id: &str) -> Value {
    json!({
        "type": "mention",
        "mention": {
            "type": "database",
            "database": { "id": database_id }
        },
        "plain_text": label
    })
}

fn date_mention_part(start: &str) -> Value {
    json!({
        "type": "mention",
        "mention": {
            "type": "date",
            "date": { "start": start }
        },
        "plain_text": start
    })
}

fn user_mention_part(user_id: &str) -> Value {
    json!({
        "type": "mention",
        "mention": {
            "type": "user",
            "user": { "id": user_id }
        },
        "plain_text": "@user"
    })
}

#[allow(clippy::too_many_arguments)]
fn database_row_properties(
    notes: &str,
    points: &str,
    status: &str,
    state: &str,
    done: bool,
    url: &str,
    people_user_ids: &[&str],
    related_page_ids: &[&str],
) -> serde_json::Map<String, Value> {
    let mut properties = serde_json::Map::from_iter([
        (
            "Notes".to_string(),
            json!({ "rich_text": rich_text_json(notes) }),
        ),
        (
            "Points".to_string(),
            json!({ "number": points.parse::<i64>().expect("points") }),
        ),
        (
            "Status".to_string(),
            json!({ "select": { "name": status } }),
        ),
        ("State".to_string(), json!({ "status": { "name": state } })),
        (
            "Tags".to_string(),
            json!({ "multi_select": [{ "name": "Alpha" }, { "name": "Beta" }] }),
        ),
        ("Done".to_string(), json!({ "checkbox": done })),
        (
            "Due".to_string(),
            json!({ "date": { "start": "2026-06-13" } }),
        ),
        ("URL".to_string(), json!({ "url": url })),
        (
            "Email".to_string(),
            json!({ "email": "cyclic@example.com" }),
        ),
        (
            "Phone".to_string(),
            json!({ "phone_number": "+1 415 555 0199" }),
        ),
        (
            "Files".to_string(),
            json!({
                "files": [
                    {
                        "name": "Initial file",
                        "type": "external",
                        "external": {
                            "url": "https://example.com/initial.pdf"
                        }
                    }
                ]
            }),
        ),
    ]);
    if !related_page_ids.is_empty() {
        properties.insert(
            "Related".to_string(),
            json!({
                "relation": related_page_ids
                    .iter()
                    .map(|id| json!({ "id": id }))
                    .collect::<Vec<_>>()
            }),
        );
    }
    if !people_user_ids.is_empty() {
        properties.insert(
            "People".to_string(),
            json!({
                "people": people_user_ids
                    .iter()
                    .map(|id| json!({ "id": id }))
                    .collect::<Vec<_>>()
            }),
        );
    }
    properties
}

fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files_into(root, &mut files);
    files
}

fn collect_files_into(path: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_into(&path, files);
        } else {
            files.push(path);
        }
    }
}

struct LocalMediaServer {
    url: String,
    expected_requests: usize,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<usize>>,
}

const LOCAL_MEDIA_SERVER_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

impl LocalMediaServer {
    fn new(bytes: Vec<u8>, expected_requests: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local media server");
        let url = format!(
            "http://{}/locality-e2e-image.png",
            listener.local_addr().expect("local media server addr")
        );
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("nonblocking media listener");
            let mut deadline = Instant::now() + LOCAL_MEDIA_SERVER_IDLE_TIMEOUT;
            let mut served = 0;
            while !server_stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if serve_local_media_response(stream, &bytes) {
                            served += 1;
                            deadline = Instant::now() + LOCAL_MEDIA_SERVER_IDLE_TIMEOUT;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept local media request: {error}"),
                }
            }
            served
        });

        Self {
            url,
            expected_requests,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self) -> &str {
        &self.url
    }

    fn assert_served(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let served = self
            .handle
            .take()
            .expect("local media server join handle")
            .join()
            .expect("join local media server");
        assert!(
            served >= self.expected_requests,
            "local media server should receive at least every expected download request: served {served}, expected {}",
            self.expected_requests
        );
    }
}

impl Drop for LocalMediaServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_local_media_response(mut stream: TcpStream, bytes: &[u8]) -> bool {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut request = [0_u8; 1024];
    let Ok(read) = stream.read(&mut request) else {
        return false;
    };
    let request = String::from_utf8_lossy(&request[..read]);
    if !request.starts_with("GET /locality-e2e-image.png ") {
        return false;
    }

    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    if stream.write_all(headers.as_bytes()).is_err() {
        return false;
    }
    if stream.write_all(bytes).is_err() {
        return false;
    }
    stream.flush().is_ok()
}

#[derive(Debug, Default)]
struct FakeGoogleDrive {
    files: Mutex<BTreeMap<String, DriveFile>>,
    children: Mutex<BTreeMap<String, Vec<DriveFile>>>,
    create_count: AtomicU64,
}

impl FakeGoogleDrive {
    fn with_children(self, parent_id: &str, children: Vec<DriveFile>) -> Self {
        for child in &children {
            self.files
                .lock()
                .expect("google drive files")
                .insert(child.id.clone(), child.clone());
        }
        self.children
            .lock()
            .expect("google drive children")
            .insert(parent_id.to_string(), children);
        self
    }
}

impl GoogleDriveApi for FakeGoogleDrive {
    fn get_file(&self, file_id: &str) -> locality_core::LocalityResult<DriveFile> {
        self.files
            .lock()
            .expect("google drive files")
            .get(file_id)
            .cloned()
            .ok_or_else(|| locality_core::LocalityError::RemoteNotFound(file_id.to_string()))
    }

    fn list_children(
        &self,
        parent_id: &str,
        _page_token: Option<&str>,
    ) -> locality_core::LocalityResult<DriveFileList> {
        Ok(DriveFileList {
            files: self
                .children
                .lock()
                .expect("google drive children")
                .get(parent_id)
                .cloned()
                .unwrap_or_default(),
            next_page_token: None,
        })
    }

    fn list_workspace_folders_by_name(
        &self,
        _name: &str,
        _page_token: Option<&str>,
    ) -> locality_core::LocalityResult<DriveFileList> {
        Ok(DriveFileList::default())
    }

    fn create_file(
        &self,
        request: DriveCreateFileRequest,
    ) -> locality_core::LocalityResult<DriveFile> {
        let create_number = self.create_count.fetch_add(1, Ordering::SeqCst) + 1;
        let id_prefix = if request.mime_type == DRIVE_FOLDER_MIME_TYPE {
            "created-folder"
        } else {
            "created-doc"
        };
        let parent = request
            .parents
            .first()
            .cloned()
            .unwrap_or_else(|| "root".to_string());
        let created = google_drive_file(
            &format!("{id_prefix}-{create_number}"),
            &request.name,
            &request.mime_type,
            &parent,
        );
        self.files
            .lock()
            .expect("google drive files")
            .insert(created.id.clone(), created.clone());
        self.children
            .lock()
            .expect("google drive children")
            .entry(parent)
            .or_default()
            .push(created.clone());
        Ok(created)
    }

    fn update_file(
        &self,
        _file_id: &str,
        _request: DriveUpdateFileRequest,
    ) -> locality_core::LocalityResult<DriveFile> {
        Err(locality_core::LocalityError::NotImplemented(
            "google drive fake update",
        ))
    }
}

#[derive(Debug, Default)]
struct FakeGoogleDocs {
    documents: Mutex<BTreeMap<String, GoogleDocument>>,
    get_count: AtomicU64,
    batches: Mutex<Vec<BatchUpdateDocumentRequest>>,
}

impl FakeGoogleDocs {
    fn with_document(self, document: GoogleDocument) -> Self {
        self.documents
            .lock()
            .expect("google docs documents")
            .insert(document.document_id.clone(), document);
        self
    }

    fn get_count(&self) -> u64 {
        self.get_count.load(Ordering::SeqCst)
    }

    fn batch_count(&self) -> usize {
        self.batches.lock().expect("google docs batches").len()
    }
}

impl GoogleDocsApi for FakeGoogleDocs {
    fn get_document(&self, document_id: &str) -> locality_core::LocalityResult<GoogleDocument> {
        self.get_count.fetch_add(1, Ordering::SeqCst);
        self.documents
            .lock()
            .expect("google docs documents")
            .get(document_id)
            .cloned()
            .ok_or_else(|| locality_core::LocalityError::RemoteNotFound(document_id.to_string()))
    }

    fn batch_update_document(
        &self,
        document_id: &str,
        request: BatchUpdateDocumentRequest,
    ) -> locality_core::LocalityResult<GoogleDocument> {
        let batch_number = {
            let mut batches = self.batches.lock().expect("google docs batches");
            batches.push(request.clone());
            batches.len()
        };
        let inserted_text = request
            .requests
            .iter()
            .rev()
            .find_map(|request| match request {
                DocsRequest::InsertText { insert_text } => Some(insert_text.text.as_str()),
                _ => None,
            });

        let mut documents = self.documents.lock().expect("google docs documents");
        let title = documents
            .get(document_id)
            .map(|document| document.title.clone())
            .unwrap_or_else(|| document_id.to_string());
        let document = match inserted_text {
            Some(inserted_text) => google_document(
                document_id,
                &title,
                &format!("rev-{}", batch_number + 1),
                inserted_text,
            ),
            None => documents
                .get(document_id)
                .cloned()
                .unwrap_or_else(|| google_document(document_id, &title, "rev-1", "")),
        };
        documents.insert(document_id.to_string(), document.clone());
        Ok(document)
    }
}

fn google_drive_folder(id: &str, name: &str, parent: &str) -> DriveFile {
    google_drive_file(id, name, DRIVE_FOLDER_MIME_TYPE, parent)
}

fn google_drive_doc(id: &str, name: &str, parent: &str) -> DriveFile {
    google_drive_file(id, name, DRIVE_GOOGLE_DOC_MIME_TYPE, parent)
}

fn google_drive_file(id: &str, name: &str, mime_type: &str, parent: &str) -> DriveFile {
    DriveFile {
        id: id.to_string(),
        name: name.to_string(),
        mime_type: mime_type.to_string(),
        parents: vec![parent.to_string()],
        modified_time: Some("2026-06-25T10:00:00.000Z".to_string()),
        version: Some("7".to_string()),
        trashed: false,
    }
}

fn google_document(id: &str, title: &str, revision: &str, content: &str) -> GoogleDocument {
    serde_json::from_value(json!({
        "documentId": id,
        "title": title,
        "revisionId": revision,
        "body": {
            "content": [
                { "startIndex": 1, "endIndex": content.len() + 1, "paragraph": {
                    "elements": [{ "textRun": { "content": content } }]
                }}
            ]
        }
    }))
    .expect("google document")
}

struct FakeBrokerOAuthExchange;

impl NotionOAuthBrokerExchange for FakeBrokerOAuthExchange {
    fn exchange_code(
        &self,
        request: &NotionOAuthBrokerCodeExchange,
    ) -> Result<NotionOAuthToken, loc_cli::connect::ConnectError> {
        assert_eq!(request.session, "broker-session");
        assert_eq!(request.state, "state-1");
        assert_eq!(request.code, "oauth-code");
        assert_eq!(
            request.redirect_uri,
            "http://localhost:8757/oauth/notion/callback"
        );
        Ok(NotionOAuthToken {
            access_token: "oauth-access-token".to_string(),
            token_type: Some("bearer".to_string()),
            refresh_token: None,
            refresh_token_kind: Some("handle".to_string()),
            refresh_token_handle: Some("opaque-refresh-handle".to_string()),
            expires_in: Some(3600),
            bot_id: Some("bot-1".to_string()),
            workspace_id: Some("workspace-1".to_string()),
            workspace_name: Some("Locality".to_string()),
            workspace_icon: None,
            owner: None,
            duplicated_template_id: None,
        })
    }
}

#[derive(Clone, Debug)]
struct FakeGoogleDocsBrokerOAuthExchange;

impl GoogleDocsOAuthBrokerExchange for FakeGoogleDocsBrokerOAuthExchange {
    fn exchange_code(
        &self,
        request: &OAuthBrokerCodeExchange,
    ) -> Result<OAuthBrokerToken, loc_cli::connect::ConnectError> {
        assert_eq!(request.connector, "google-docs");
        assert_eq!(request.session, "google-broker-session");
        assert_eq!(request.state, "google-state-1");
        assert_eq!(request.code, "google-oauth-code");
        assert_eq!(
            request.redirect_uri,
            "http://localhost:8757/oauth/google-docs/callback"
        );
        Ok(OAuthBrokerToken {
            access_token: "google-oauth-access-token".to_string(),
            token_type: Some("Bearer".to_string()),
            expires_in: Some(3600),
            refresh_token_handle: Some("google-opaque-refresh-handle".to_string()),
            account_id: Some("acct-1".to_string()),
            account_label: Some("user@example.com".to_string()),
            workspace_id: Some("google-drive".to_string()),
            workspace_name: Some("Google Drive".to_string()),
            scopes: GOOGLE_DOCS_OAUTH_SCOPES
                .iter()
                .map(|scope| scope.to_string())
                .collect(),
        })
    }
}

#[derive(Debug)]
struct RecursivePageDirectoryNotionApi {
    pages: BTreeMap<String, PageDto>,
    children: BTreeMap<String, BlockListDto>,
}

impl RecursivePageDirectoryNotionApi {
    fn new() -> Self {
        Self {
            pages: BTreeMap::from([
                ("project-page".to_string(), page("project-page", "Project")),
                (
                    "design-notes-page".to_string(),
                    page("design-notes-page", "Design Notes"),
                ),
                (
                    "appendix-page".to_string(),
                    page("appendix-page", "Appendix"),
                ),
            ]),
            children: BTreeMap::from([
                (
                    "project-page".to_string(),
                    PaginatedListDto {
                        results: vec![
                            paragraph_block("project-paragraph", "Project root body."),
                            child_page_block("design-notes-page", "Design Notes"),
                        ],
                        next_cursor: None,
                        has_more: false,
                    },
                ),
                (
                    "design-notes-page".to_string(),
                    PaginatedListDto {
                        results: vec![
                            paragraph_block("design-paragraph", "Design notes child body."),
                            child_page_block("appendix-page", "Appendix"),
                        ],
                        next_cursor: None,
                        has_more: false,
                    },
                ),
                (
                    "appendix-page".to_string(),
                    PaginatedListDto {
                        results: vec![paragraph_block(
                            "appendix-paragraph",
                            "Appendix nested child body.",
                        )],
                        next_cursor: None,
                        has_more: false,
                    },
                ),
            ]),
        }
    }
}

impl NotionApi for RecursivePageDirectoryNotionApi {
    fn retrieve_page(&self, page_id: &str) -> locality_core::LocalityResult<PageDto> {
        self.pages.get(page_id).cloned().ok_or_else(|| {
            locality_core::LocalityError::InvalidState(format!("missing page {page_id}"))
        })
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        _start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<BlockListDto> {
        Ok(self.children.get(block_id).cloned().unwrap_or_default())
    }

    fn search_pages(
        &self,
        _start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<PageListDto> {
        Ok(PaginatedListDto {
            results: self.pages.values().cloned().collect(),
            next_cursor: None,
            has_more: false,
        })
    }

    fn update_block(
        &self,
        _block_id: &str,
        _body: Value,
    ) -> locality_core::LocalityResult<BlockDto> {
        Err(locality_core::LocalityError::NotImplemented(
            "recursive page directory fixture update block",
        ))
    }

    fn append_block_children(
        &self,
        _block_id: &str,
        _body: Value,
    ) -> locality_core::LocalityResult<BlockListDto> {
        Err(locality_core::LocalityError::NotImplemented(
            "recursive page directory fixture append block children",
        ))
    }

    fn delete_block(&self, _block_id: &str) -> locality_core::LocalityResult<BlockDto> {
        Err(locality_core::LocalityError::NotImplemented(
            "recursive page directory fixture delete block",
        ))
    }
}

#[derive(Debug)]
struct MutableNotionApi {
    page: Mutex<PageDto>,
    blocks: Mutex<Vec<BlockDto>>,
    database: Mutex<Option<DatabaseDto>>,
    data_source: Mutex<Option<DataSourceDto>>,
    block_children_calls: AtomicUsize,
    append_count: Mutex<usize>,
    calls: Mutex<Vec<WriteCall>>,
}

impl MutableNotionApi {
    fn new() -> Self {
        Self::with_blocks(vec![
            paragraph_block("block-1", "First paragraph."),
            synced_block("synced-1", "source-block-1"),
            paragraph_block("block-2", "Second paragraph."),
            paragraph_block("block-3", "Third paragraph."),
            paragraph_block("block-4", "Fourth paragraph."),
            paragraph_block("block-5", "Fifth paragraph."),
            paragraph_block("block-6", "Sixth paragraph."),
        ])
    }

    fn with_blocks(blocks: Vec<BlockDto>) -> Self {
        Self::with_page_and_blocks(page("page-1", "Initial Idea"), blocks)
    }

    fn with_page_and_blocks(page: PageDto, blocks: Vec<BlockDto>) -> Self {
        Self {
            page: Mutex::new(page),
            blocks: Mutex::new(blocks),
            database: Mutex::new(None),
            data_source: Mutex::new(None),
            block_children_calls: AtomicUsize::new(0),
            append_count: Mutex::new(0),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn with_page_blocks_and_database_schema(
        page: PageDto,
        blocks: Vec<BlockDto>,
        database: DatabaseDto,
        data_source: DataSourceDto,
    ) -> Self {
        Self {
            page: Mutex::new(page),
            blocks: Mutex::new(blocks),
            database: Mutex::new(Some(database)),
            data_source: Mutex::new(Some(data_source)),
            block_children_calls: AtomicUsize::new(0),
            append_count: Mutex::new(0),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn block_children_count(&self) -> usize {
        self.block_children_calls.load(Ordering::SeqCst)
    }
}

impl NotionApi for MutableNotionApi {
    fn retrieve_page(&self, page_id: &str) -> locality_core::LocalityResult<PageDto> {
        let page = self.page.lock().expect("page");
        if page_id == page.id {
            Ok(page.clone())
        } else {
            Err(locality_core::LocalityError::InvalidState(format!(
                "missing page {page_id}"
            )))
        }
    }

    fn retrieve_database(&self, database_id: &str) -> locality_core::LocalityResult<DatabaseDto> {
        let database = self.database.lock().expect("database");
        match database
            .as_ref()
            .filter(|database| database.id == database_id)
        {
            Some(database) => Ok(database.clone()),
            None => Err(locality_core::LocalityError::InvalidState(format!(
                "missing database {database_id}"
            ))),
        }
    }

    fn retrieve_data_source(
        &self,
        data_source_id: &str,
    ) -> locality_core::LocalityResult<DataSourceDto> {
        let data_source = self.data_source.lock().expect("data source");
        match data_source
            .as_ref()
            .filter(|data_source| data_source.id == data_source_id)
        {
            Some(data_source) => Ok(data_source.clone()),
            None => Err(locality_core::LocalityError::InvalidState(format!(
                "missing data source {data_source_id}"
            ))),
        }
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        _start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<BlockListDto> {
        self.block_children_calls.fetch_add(1, Ordering::SeqCst);
        let page_id = self.page.lock().expect("page").id.clone();
        if block_id == page_id {
            Ok(PaginatedListDto {
                results: self.blocks.lock().expect("blocks").clone(),
                next_cursor: None,
                has_more: false,
            })
        } else {
            Ok(PaginatedListDto::default())
        }
    }

    fn search_pages(
        &self,
        _start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<PageListDto> {
        let page = self.page.lock().expect("page").clone();
        Ok(PaginatedListDto {
            results: vec![page],
            next_cursor: None,
            has_more: false,
        })
    }

    fn update_page(&self, page_id: &str, body: Value) -> locality_core::LocalityResult<PageDto> {
        self.calls
            .lock()
            .expect("calls")
            .push(WriteCall::UpdatePage {
                page_id: page_id.to_string(),
                body: body.clone(),
            });
        let mut page = self.page.lock().expect("page");
        if page.id != page_id {
            return Err(locality_core::LocalityError::InvalidState(format!(
                "missing page {page_id}"
            )));
        }
        if let Some(properties) = body.get("properties").and_then(Value::as_object) {
            for (name, patch) in properties {
                if let Some(property) = page.properties.get_mut(name) {
                    apply_mutable_page_property_patch(property, patch);
                }
            }
        }
        page.last_edited_time = Some("2026-06-10T00:00:01.000Z".to_string());
        Ok(page.clone())
    }

    fn move_page(&self, page_id: &str, parent: Value) -> locality_core::LocalityResult<PageDto> {
        self.calls.lock().expect("calls").push(WriteCall::MovePage {
            page_id: page_id.to_string(),
            parent: parent.clone(),
        });
        let mut page = self.page.lock().expect("page");
        if page.id != page_id {
            return Err(locality_core::LocalityError::InvalidState(format!(
                "missing page {page_id}"
            )));
        }
        page.parent = Some(ParentDto {
            kind: parent
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            page_id: parent
                .get("page_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            data_source_id: parent
                .get("data_source_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            ..ParentDto::default()
        });
        page.last_edited_time = Some("2026-06-10T00:00:01.000Z".to_string());
        Ok(page.clone())
    }

    fn update_block(&self, block_id: &str, body: Value) -> locality_core::LocalityResult<BlockDto> {
        self.calls.lock().expect("calls").push(WriteCall::Update {
            block_id: block_id.to_string(),
        });
        let text = body_text(&body).unwrap_or_default();
        let mut blocks = self.blocks.lock().expect("blocks");
        if let Some(block) = blocks.iter_mut().find(|block| block.id == block_id) {
            *block = paragraph_block(block_id, &text);
            return Ok(block.clone());
        }
        Ok(paragraph_block(block_id, &text))
    }

    fn move_block(
        &self,
        block_id: &str,
        _parent_id: &str,
        after: Option<&str>,
    ) -> locality_core::LocalityResult<BlockDto> {
        self.calls.lock().expect("calls").push(WriteCall::Move {
            block_id: block_id.to_string(),
            after: after.map(str::to_string),
        });
        let mut blocks = self.blocks.lock().expect("blocks");
        let Some(index) = blocks.iter().position(|block| block.id == block_id) else {
            return Ok(paragraph_block(block_id, ""));
        };
        let block = blocks.remove(index);
        let insert_at = after
            .and_then(|after| blocks.iter().position(|block| block.id == after))
            .map_or(0, |index| index + 1);
        blocks.insert(insert_at, block.clone());
        Ok(block)
    }

    fn append_block_children(
        &self,
        block_id: &str,
        body: Value,
    ) -> locality_core::LocalityResult<BlockListDto> {
        self.calls.lock().expect("calls").push(WriteCall::Append {
            parent_id: block_id.to_string(),
        });
        let mut append_count = self.append_count.lock().expect("append count");
        *append_count += 1;
        let created_id = format!("created-{}", *append_count);
        let block = appended_block_from_body(&created_id, &body)
            .unwrap_or_else(|| paragraph_block(&created_id, "Created."));
        let after = body
            .pointer("/position/after_block/id")
            .and_then(serde_json::Value::as_str);
        let mut blocks = self.blocks.lock().expect("blocks");
        let insert_at = after
            .and_then(|after| blocks.iter().position(|block| block.id == after))
            .map_or(0, |index| index + 1);
        blocks.insert(insert_at, block.clone());
        Ok(PaginatedListDto {
            results: vec![block],
            next_cursor: None,
            has_more: false,
        })
    }

    fn delete_block(&self, block_id: &str) -> locality_core::LocalityResult<BlockDto> {
        self.calls.lock().expect("calls").push(WriteCall::Delete {
            block_id: block_id.to_string(),
        });
        let mut blocks = self.blocks.lock().expect("blocks");
        if let Some(index) = blocks.iter().position(|block| block.id == block_id) {
            return Ok(blocks.remove(index));
        }
        Ok(paragraph_block(block_id, ""))
    }
}

fn apply_mutable_page_property_patch(property: &mut PagePropertyDto, patch: &Value) {
    match property.kind.as_str() {
        "number" => {
            property.number = patch.get("number").and_then(Value::as_number).cloned();
        }
        "select" => {
            property.select = patch.get("select").and_then(|value| {
                if value.is_null() {
                    None
                } else {
                    serde_json::from_value(value.clone()).ok().or_else(|| {
                        value
                            .get("name")
                            .and_then(Value::as_str)
                            .map(|name| SelectOptionDto {
                                id: name.to_string(),
                                name: name.to_string(),
                                color: None,
                            })
                    })
                }
            });
        }
        "multi_select" => {
            property.multi_select = patch
                .get("multi_select")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default();
        }
        "date" => {
            property.date = patch.get("date").and_then(|value| {
                if value.is_null() {
                    None
                } else {
                    serde_json::from_value(value.clone()).ok()
                }
            });
        }
        "url" => {
            property.url = patch.get("url").and_then(Value::as_str).map(str::to_string);
        }
        "files" => {
            property.files = patch
                .get("files")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default();
        }
        "people" => {
            property.people = patch
                .get("people")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default();
        }
        "relation" => {
            property.relation = patch
                .get("relation")
                .and_then(|value| serde_json::from_value(value.clone()).ok())
                .unwrap_or_default();
        }
        _ => {}
    }
}

fn replace_mutable_paragraph(api: &Arc<MutableNotionApi>, block_id: &str, text: &str) {
    let mut blocks = api.blocks.lock().expect("blocks");
    let block = blocks
        .iter_mut()
        .find(|block| block.id == block_id)
        .expect("mutable paragraph block");
    *block = paragraph_block(block_id, text);
}

fn replace_mutable_page_title_and_version(api: &Arc<MutableNotionApi>, title: &str, version: &str) {
    let mut page = api.page.lock().expect("page");
    page.properties.insert(
        "title".to_string(),
        PagePropertyDto {
            kind: "title".to_string(),
            title: vec![rich_text(title)],
            ..Default::default()
        },
    );
    page.last_edited_time = Some(version.to_string());
}

fn observe_job(mount_id: &MountId, remote_id: &RemoteId) -> SyncJob {
    SyncJob::new(
        mount_id.clone(),
        Some(remote_id.clone()),
        SyncJobKind::ObserveEntity,
        ChangeHintKind::RemoteMaybeChanged,
    )
}

fn remote_observation(
    mount_id: &MountId,
    remote_id: &RemoteId,
    deleted: bool,
    remote_version: &str,
) -> RemoteObservation {
    RemoteObservation {
        mount_id: mount_id.clone(),
        remote_id: remote_id.clone(),
        kind: EntityKind::Page,
        title: "Roadmap".to_string(),
        parent_remote_id: None,
        projected_path: PathBuf::from("Roadmap/page.md"),
        remote_version: Some(RemoteVersion::new(remote_version)),
        deleted,
        raw_metadata_json: "{}".to_string(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WriteCall {
    UpdatePage {
        page_id: String,
        body: Value,
    },
    Update {
        block_id: String,
    },
    Append {
        parent_id: String,
    },
    Move {
        block_id: String,
        after: Option<String>,
    },
    MovePage {
        page_id: String,
        parent: Value,
    },
    Delete {
        block_id: String,
    },
}

#[derive(Default)]
struct BlockingGuardrailConnector {
    concurrency_checks: AtomicU64,
    apply_calls: AtomicU64,
}

impl Connector for BlockingGuardrailConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("guardrail-test")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: true,
            supports_databases: false,
            supports_oauth: false,
            supports_remote_observation: false,
            supports_lazy_child_enumeration: false,
            supports_media_download: false,
            supports_undo: false,
            supports_batch_observation: false,
        }
    }

    fn enumerate(
        &self,
        _request: EnumerateRequest,
    ) -> locality_core::LocalityResult<Vec<TreeEntry>> {
        Err(locality_core::LocalityError::NotImplemented(
            "guardrail test enumerate",
        ))
    }

    fn fetch(&self, _request: FetchRequest) -> locality_core::LocalityResult<NativeEntity> {
        Err(locality_core::LocalityError::NotImplemented(
            "guardrail test fetch",
        ))
    }

    fn render(&self, _entity: &NativeEntity) -> locality_core::LocalityResult<CanonicalDocument> {
        Err(locality_core::LocalityError::NotImplemented(
            "guardrail test render",
        ))
    }

    fn parse(&self, _document: &CanonicalDocument) -> locality_core::LocalityResult<ParsedEntity> {
        Err(locality_core::LocalityError::NotImplemented(
            "guardrail test parse",
        ))
    }

    fn check_concurrency(
        &self,
        _request: ApplyPlanRequest<'_>,
    ) -> locality_core::LocalityResult<()> {
        self.concurrency_checks.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn apply(
        &self,
        request: ApplyPlanRequest<'_>,
    ) -> locality_core::LocalityResult<ApplyPlanResult> {
        self.apply_calls.fetch_add(1, Ordering::Relaxed);
        Ok(ApplyPlanResult {
            changed_remote_ids: request.plan.affected_entities.clone(),
            effects: Vec::new(),
        })
    }

    fn apply_undo(
        &self,
        _request: ApplyUndoRequest<'_>,
    ) -> locality_core::LocalityResult<ApplyUndoResult> {
        Err(locality_core::LocalityError::NotImplemented(
            "guardrail test undo",
        ))
    }
}

impl HydrationSource for BlockingGuardrailConnector {
    fn fetch_render(
        &self,
        _request: &HydrationRequest,
    ) -> locality_core::LocalityResult<HydratedEntity> {
        Err(locality_core::LocalityError::NotImplemented(
            "guardrail test fetch render",
        ))
    }
}

#[derive(Debug)]
struct PropertyOnlyConnector {
    remote_frontmatter: Mutex<String>,
    edited_frontmatter: String,
    body: String,
    apply_calls: AtomicU64,
}

impl PropertyOnlyConnector {
    fn new(original_frontmatter: &str, edited_frontmatter: &str, body: &str) -> Self {
        Self {
            remote_frontmatter: Mutex::new(original_frontmatter.to_string()),
            edited_frontmatter: edited_frontmatter.to_string(),
            body: body.to_string(),
            apply_calls: AtomicU64::new(0),
        }
    }

    fn apply_count(&self) -> u64 {
        self.apply_calls.load(Ordering::SeqCst)
    }
}

impl Connector for PropertyOnlyConnector {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind("property-test")
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supports_block_updates: true,
            supports_databases: true,
            supports_oauth: false,
            supports_remote_observation: false,
            supports_lazy_child_enumeration: false,
            supports_media_download: false,
            supports_undo: true,
            supports_batch_observation: false,
        }
    }

    fn enumerate(
        &self,
        _request: EnumerateRequest,
    ) -> locality_core::LocalityResult<Vec<TreeEntry>> {
        Err(locality_core::LocalityError::NotImplemented(
            "property test enumerate",
        ))
    }

    fn fetch(&self, _request: FetchRequest) -> locality_core::LocalityResult<NativeEntity> {
        Err(locality_core::LocalityError::NotImplemented(
            "property test fetch",
        ))
    }

    fn render(&self, _entity: &NativeEntity) -> locality_core::LocalityResult<CanonicalDocument> {
        Err(locality_core::LocalityError::NotImplemented(
            "property test render",
        ))
    }

    fn parse(&self, _document: &CanonicalDocument) -> locality_core::LocalityResult<ParsedEntity> {
        Err(locality_core::LocalityError::NotImplemented(
            "property test parse",
        ))
    }

    fn check_concurrency(
        &self,
        _request: ApplyPlanRequest<'_>,
    ) -> locality_core::LocalityResult<()> {
        Ok(())
    }

    fn apply(
        &self,
        request: ApplyPlanRequest<'_>,
    ) -> locality_core::LocalityResult<ApplyPlanResult> {
        self.apply_calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            request.plan.operations.iter().all(|operation| matches!(
                operation,
                locality_core::planner::PushOperation::UpdateProperties { .. }
            )),
            "property-only test connector received non-property operations: {:#?}",
            request.plan.operations
        );
        *self
            .remote_frontmatter
            .lock()
            .expect("property frontmatter") = self.edited_frontmatter.clone();
        Ok(ApplyPlanResult {
            changed_remote_ids: request.plan.affected_entities.clone(),
            effects: Vec::new(),
        })
    }

    fn apply_undo(
        &self,
        _request: ApplyUndoRequest<'_>,
    ) -> locality_core::LocalityResult<ApplyUndoResult> {
        panic!("property-only undo should block before reverse apply");
    }
}

impl HydrationSource for PropertyOnlyConnector {
    fn fetch_render(
        &self,
        request: &HydrationRequest,
    ) -> locality_core::LocalityResult<HydratedEntity> {
        let frontmatter = self
            .remote_frontmatter
            .lock()
            .expect("property frontmatter")
            .clone();
        let document = CanonicalDocument::new(frontmatter.clone(), self.body.clone());
        let shadow = ShadowDocument::from_synced_body(
            request.remote_id.clone(),
            self.body.clone(),
            10,
            [RemoteId::new("block-1")],
        )
        .expect("property rendered shadow")
        .with_frontmatter(frontmatter);
        Ok(HydratedEntity {
            document,
            shadow,
            remote_edited_at: Some("2026-06-10T00:00:01.000Z".to_string()),
            assets: Vec::new(),
        })
    }
}

fn page(id: &str, title: &str) -> PageDto {
    PageDto {
        id: id.to_string(),
        parent: None,
        created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        last_edited_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        archived: false,
        in_trash: false,
        properties: BTreeMap::from([(
            "title".to_string(),
            PagePropertyDto {
                kind: "title".to_string(),
                title: vec![rich_text(title)],
                ..Default::default()
            },
        )]),
    }
}

fn paragraph_block(id: &str, text: &str) -> BlockDto {
    let mut block = BlockDto {
        id: id.to_string(),
        kind: "paragraph".to_string(),
        ..Default::default()
    };
    block.paragraph = Some(RichTextBlockDto {
        rich_text: vec![rich_text(text)],
        color: None,
    });
    block
}

fn child_page_block(id: &str, title: &str) -> BlockDto {
    let mut block = BlockDto {
        id: id.to_string(),
        kind: "child_page".to_string(),
        ..Default::default()
    };
    block.child_page = Some(TitleBlockDto {
        title: title.to_string(),
    });
    block
}

fn synced_block(id: &str, source_block_id: &str) -> BlockDto {
    let mut block = BlockDto {
        id: id.to_string(),
        kind: "synced_block".to_string(),
        ..Default::default()
    };
    block.synced_block = Some(SyncedBlockDto {
        synced_from: Some(SyncedFromDto {
            kind: "block_id".to_string(),
            block_id: Some(source_block_id.to_string()),
        }),
    });
    block
}

fn media_block(id: &str, kind: &str, url: &str, caption: &str) -> BlockDto {
    let mut block = media_child(kind, url, caption);
    block["id"] = json!(id);
    serde_json::from_value(block).expect("media block dto")
}

fn ambiguous_tasks_schema() -> &'static str {
    r#"loc:
  type: notion_database_schema
  database_id: "database-1"
title: "Tasks"
data_sources:
  - id: "source-1"
    name: "Tasks A"
    properties:
      Name:
        id: "name-a"
        type: "title"
      Status:
        id: "status-a"
        type: "select"
        options:
          - name: "Todo"
            id: "todo-a"
  - id: "source-2"
    name: "Tasks B"
    properties:
      Name:
        id: "name-b"
        type: "title"
      Status:
        id: "status-b"
        type: "select"
        options:
          - name: "Todo"
            id: "todo-b"
"#
}

fn optional_property_tasks_schema() -> &'static str {
    r#"loc:
  type: notion_database_schema
  database_id: "database-1"
title: "Tasks"
data_sources:
  - id: "source-1"
    name: "Tasks"
    properties:
      Name:
        id: "name"
        type: "title"
      Status:
        id: "status"
        type: "select"
        options:
          - name: "Todo"
            id: "todo"
      Tags:
        id: "tags"
        type: "multi_select"
        options:
          - name: "Alpha"
            id: "alpha"
      Points:
        id: "points"
        type: "number"
      Due:
        id: "due"
        type: "date"
      URL:
        id: "url"
        type: "url"
      Files:
        id: "files"
        type: "files"
      People:
        id: "people"
        type: "people"
      Related:
        id: "related"
        type: "relation"
"#
}

fn read_only_property_tasks_schema() -> &'static str {
    r#"loc:
  type: notion_database_schema
  database_id: "database-1"
title: "Tasks"
data_sources:
  - id: "source-1"
    name: "Tasks"
    properties:
      Name:
        id: "name"
        type: "title"
      Status:
        id: "status"
        type: "select"
        options:
          - name: "Todo"
            id: "todo"
      Formula:
        id: "formula"
        type: "formula"
"#
}

fn appended_block_from_body(id: &str, body: &Value) -> Option<BlockDto> {
    let child = body.pointer("/children/0")?;
    let kind = child.get("type")?.as_str()?;
    if kind == "synced_block" {
        let mut block = BlockDto {
            id: id.to_string(),
            kind: kind.to_string(),
            ..Default::default()
        };
        block.synced_block = Some(
            serde_json::from_value(child.get(kind)?.clone()).unwrap_or_else(|_| Default::default()),
        );
        return Some(block);
    }

    let text = body_text(body).unwrap_or_else(|| "Created.".to_string());
    Some(paragraph_block(id, &text))
}

fn rich_text(text: &str) -> RichTextDto {
    RichTextDto {
        kind: "text".to_string(),
        text: Some(TextRichTextDto {
            content: text.to_string(),
            link: None,
        }),
        mention: None,
        equation: None,
        plain_text: text.to_string(),
        href: None,
        annotations: Default::default(),
    }
}

fn body_text(body: &Value) -> Option<String> {
    body.pointer("/paragraph/rich_text/0/text/content")
        .or_else(|| body.pointer("/children/0/paragraph/rich_text/0/text/content"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn normalize_notion_id(input: &str) -> String {
    let trimmed = input.trim().trim_end_matches('/');
    let candidate = trimmed
        .split(['?', '#'])
        .next()
        .unwrap_or(trimmed)
        .rsplit('/')
        .next()
        .unwrap_or(trimmed);
    let hex = candidate
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>();
    if hex.len() >= 32 {
        hex[hex.len() - 32..].to_string()
    } else {
        candidate.to_string()
    }
}

fn compact_notion_id(input: &str) -> String {
    input
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn notion_object_url(id: &str) -> String {
    format!("https://www.notion.so/{}", normalize_notion_id(id))
}

fn notion_pretty_workspace_url(workspace_slug: &str, title: &str, id: &str) -> String {
    format!(
        "https://app.notion.com/p/{}/{}-{}",
        workspace_slug,
        slug_for_test(title),
        normalize_notion_id(id)
    )
}

fn slug_for_test(title: &str) -> String {
    let slug = title
        .chars()
        .filter_map(|character| {
            if character.is_ascii_alphanumeric() {
                Some(character.to_ascii_lowercase())
            } else if character.is_whitespace() || matches!(character, '-' | '_') {
                Some('-')
            } else {
                None
            }
        })
        .collect::<String>();
    slug.trim_matches('-').to_string()
}

fn replace_line_with_prefix(markdown: String, prefix: &str, replacement: &str) -> String {
    let mut replaced = false;
    let lines = markdown
        .lines()
        .map(|line| {
            if !replaced && line.starts_with(prefix) {
                replaced = true;
                replacement.to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>();

    assert!(
        replaced,
        "expected line starting with `{prefix}` in:\n{markdown}"
    );

    let trailing_newline = if markdown.ends_with('\n') { "\n" } else { "" };
    format!("{}{trailing_newline}", lines.join("\n"))
}

fn markdown_image_line<'a>(markdown: &'a str, caption: &str) -> &'a str {
    let prefix = format!("![{caption}](");
    markdown
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("missing image line for {caption:?} in:\n{markdown}"))
}

fn markdown_link_line<'a>(markdown: &'a str, caption: &str) -> &'a str {
    let prefix = format!("[{caption}](");
    markdown
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("missing link line for {caption:?} in:\n{markdown}"))
}

fn markdown_link_href(line: &str) -> &str {
    let label_start = if line.starts_with("![") {
        2
    } else if line.starts_with('[') {
        1
    } else {
        panic!("markdown link must start with `[` or `![`: {line:?}");
    };
    let label_end = find_markdown_link_label_end(line, label_start);
    let href_start = label_end + 2;
    let href_end = find_markdown_link_href_end(line, href_start);
    &line[href_start..href_end]
}

fn find_markdown_link_label_end(input: &str, start: usize) -> usize {
    let mut escaped = false;
    for (index, ch) in input.char_indices().skip_while(|(index, _)| *index < start) {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == ']' && input[index + ch.len_utf8()..].starts_with('(') {
            return index;
        }
    }
    panic!("markdown link label is not closed: {input:?}");
}

fn find_markdown_link_href_end(input: &str, href_start: usize) -> usize {
    let mut escaped = false;
    let mut paren_depth = 0usize;
    for (offset, ch) in input[href_start..].char_indices() {
        let index = href_start + offset;
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match ch {
            '(' => paren_depth += 1,
            ')' if paren_depth == 0 => return index,
            ')' => paren_depth -= 1,
            _ => {}
        }
    }
    panic!("markdown link href is not closed: {input:?}");
}

fn assert_local_image_markdown(markdown: &str, caption: &str) {
    let line = markdown_image_line(markdown, caption);
    assert_local_media_href(line, caption);
}

fn assert_local_media_link_markdown(markdown: &str, caption: &str) {
    let line = markdown_link_line(markdown, caption);
    assert_local_media_href(line, caption);
}

fn assert_local_media_href(line: &str, caption: &str) {
    let href = markdown_link_href(line);
    assert!(
        !href.starts_with("http://")
            && !href.starts_with("https://")
            && href.contains(".loc/media/"),
        "expected local media href for {caption:?}, got {line:?}"
    );
}

fn local_image_path(root: &Path, page_path: &Path, markdown: &str, caption: &str) -> PathBuf {
    let line = markdown_image_line(markdown, caption);
    local_media_path_from_line(root, page_path, line)
}

fn local_media_link_path(root: &Path, page_path: &Path, markdown: &str, caption: &str) -> PathBuf {
    let line = markdown_link_line(markdown, caption);
    local_media_path_from_line(root, page_path, line)
}

fn local_media_path_from_line(root: &Path, page_path: &Path, line: &str) -> PathBuf {
    let href = markdown_link_href(line);
    let relative_page = page_path
        .strip_prefix(root)
        .unwrap_or_else(|_| panic!("page path {page_path:?} is not under root {root:?}"));
    let local_path = resolve_media_href_with_content_root(relative_page, href, root)
        .unwrap_or_else(|| panic!("image href {href:?} is not a local media href"));
    root.join(local_path)
}

fn tiny_mp4_bytes() -> &'static [u8] {
    b"\0\0\0\x18ftypmp42\0\0\0\0mp42isom\0\0\0\x08free\0\0\0\x08mdat"
}

fn tiny_pdf_bytes() -> &'static [u8] {
    b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n"
}

fn tiny_wav_bytes() -> &'static [u8] {
    b"RIFF$\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0@\x1f\0\0@\x1f\0\0\x01\0\x08\0data\0\0\0\0"
}

fn tiny_html_bytes() -> &'static [u8] {
    b"<!doctype html><title>Locality upload</title><p>hello</p>\n"
}

fn tiny_png_bytes() -> &'static [u8] {
    &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

fn timestamp_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn unique_id_prefix() -> String {
    let mut value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let first_alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut prefix = String::new();
    let first_index = (value % first_alphabet.len() as u128) as usize;
    prefix.push(first_alphabet[first_index] as char);
    value /= first_alphabet.len() as u128;
    for _ in 0..6 {
        let index = (value % alphabet.len() as u128) as usize;
        prefix.push(alphabet[index] as char);
        value /= alphabet.len() as u128;
    }
    prefix
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
}
