use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_cli::diff::run_diff;
use afs_cli::history::{LogOptions, run_log, run_undo_with_applier};
use afs_cli::inspect::{InspectOptions, run_inspect};
use afs_cli::mount::{MountOptions, run_mount};
use afs_cli::pull::run_pull;
use afs_cli::push::{PushOptions, run_push_with_daemon, run_push_with_daemon_at_state_root};
use afs_cli::search::{SearchOptions, run_search};
use afs_cli::status::{StatusOptions, run_status};
use afs_connector::{Connector, ConnectorUndoApplier, FetchRequest};
use afs_core::canonical::render_canonical_markdown;
use afs_core::conflict::{
    CONFLICT_LOCAL_MARKER, CONFLICT_REMOTE_MARKER, CONFLICT_SEPARATOR_MARKER,
    has_unresolved_conflict_markers,
};
use afs_core::explain::{RemoteChangeAction, RemoteChangeState};
use afs_core::hydration::{HydrationPolicy, HydrationReason, HydrationRequest};
use afs_core::model::{HydrationState, MountId, RemoteId};
use afs_notion::client::{HttpNotionApi, NotionApi};
use afs_notion::dto::{
    BlockDto, BlockListDto, DatabaseDto, NotionPageBundle, PageDto, PageListDto, PagePropertyDto,
    PaginatedListDto, RichTextBlockDto, RichTextDto, SyncedBlockDto, SyncedFromDto,
    TextRichTextDto,
};
use afs_notion::media::resolve_media_href_with_content_root;
use afs_notion::{NotionConfig, NotionConnector};
use afs_store::{
    ConnectionId, EntityRepository, InMemoryStateStore, JournalRepository, MountRepository,
    ProjectionMode, VirtualMutationRepository,
};
use afsd::hydration::{HydrationExecutor, HydrationOutcome, HydrationQueue};
use afsd::reconcile::{DefaultFetchScheduleStrategy, reconcile_scheduled_pull};
use afsd::scheduler::PullScheduler;
use afsd::virtual_fs::{
    ROOT_CONTAINER_IDENTIFIER, materialize_virtual_fs_item_with_content_root,
    refresh_virtual_fs_children, rename_virtual_fs_item, source_root_identifier,
    trash_virtual_fs_item, virtual_fs_children_with_content_root, virtual_fs_content_root,
};
use serde_json::{Value, json};
use std::time::Duration;

const LIVE_PARENT_ENV: &str = "AFS_NOTION_LIVE_PARENT_PAGE";
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_scratch_page_mount_edit_push_verifies_notion() {
    std::env::var(TOKEN_ENV).expect("NOTION_TOKEN");
    let parent_page = normalize_notion_id(
        &std::env::var(LIVE_PARENT_ENV)
            .unwrap_or_else(|_| panic!("set {LIVE_PARENT_ENV} to a writable page ID or URL")),
    );
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let marker = format!("AFS live mounted edit {}", unique_suffix());
    let scratch = cleanup.create_page(
        &parent_page,
        &format!("AFS mounted e2e {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": {
                "rich_text": [
                    {
                        "type": "text",
                        "text": {
                            "content": "Original paragraph created by the mounted AFS live e2e test."
                        }
                    }
                ]
            }
        })],
    );

    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let connector = NotionConnector::new(NotionConfig::default());

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
        },
    )
    .expect("mount");
    run_pull(&mut store, &connector, &fixture.root).expect("pull");
    let page_path = fixture.page_file();
    let original = fs::read_to_string(&page_path).expect("read pulled page");
    assert!(original.contains("Original paragraph created by the mounted AFS live e2e test."));
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_block_type_replace_pushes_and_reconciles_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let original_text = "Replace block paragraph original.";
    let untouched_text = "Paragraph after replacement should remain.";
    let replacement_text = "Replacement bullet from live replace";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live replace block {}", unique_suffix()),
        vec![
            paragraph_child(original_text),
            paragraph_child(untouched_text),
        ],
    );
    let original_block_id = first_live_child_block_id(&cleanup.api, &scratch.id);
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_directive_block_move_pushes_and_reconciles_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let anchor_text = "Move anchor paragraph.";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live directive move {}", unique_suffix()),
        vec![
            paragraph_child(anchor_text),
            json!({
                "object": "block",
                "type": "table_of_contents",
                "table_of_contents": { "color": "default" }
            }),
        ],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_directive_block_move_undo_restores_remote_order() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let anchor_text = "Undo move anchor paragraph.";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live directive move undo {}", unique_suffix()),
        vec![
            paragraph_child(anchor_text),
            json!({
                "object": "block",
                "type": "table_of_contents",
                "table_of_contents": { "color": "default" }
            }),
        ],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_link_to_page_line_move_preserves_notion_block_type() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let target = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live link move target {}", unique_suffix()),
        vec![paragraph_child("Target for link_to_page move.")],
    );
    let anchor_text = "Anchor before link_to_page.";
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live link move source {}", unique_suffix()),
        vec![
            paragraph_child(anchor_text),
            json!({
                "object": "block",
                "type": "link_to_page",
                "link_to_page": { "type": "page_id", "page_id": target.id }
            }),
        ],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_link_to_page_retarget_blocks_before_journaled_apply() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let original_target = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live link retarget original {}", unique_suffix()),
        vec![paragraph_child(
            "Original target for link_to_page retarget.",
        )],
    );
    let replacement_target = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live link retarget replacement {}", unique_suffix()),
        vec![paragraph_child(
            "Replacement target for link_to_page retarget.",
        )],
    );
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live link retarget source {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "link_to_page",
            "link_to_page": { "type": "page_id", "page_id": original_target.id }
        })],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_notion_link_labeled_like_link_to_page_can_be_edited() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let original_target = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live paragraph link original {}", unique_suffix()),
        vec![paragraph_child("Original target for paragraph link.")],
    );
    let replacement_target = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live paragraph link replacement {}", unique_suffix()),
        vec![paragraph_child("Replacement target for paragraph link.")],
    );
    let original_url = notion_object_url(&original_target.id);
    let replacement_url = notion_object_url(&replacement_target.id);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live paragraph link source {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": { "rich_text": [linked_text_part("Linked page", &original_url)] }
        })],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_link_with_parentheses_href_can_be_edited() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let href = "https://example.com/docs/foo)";
    let markdown_href = href.replace(')', "\\)");
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live paragraph paren link {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": { "rich_text": [linked_text_part("Paren link", href)] }
        })],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_literal_break_tag_edits_preserve_literal_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live literal break tag {}", unique_suffix()),
        vec![paragraph_child("Literal <br> tag")],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_literal_underline_tag_edits_preserve_literal_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live literal underline tag {}", unique_suffix()),
        vec![paragraph_child("Literal <u>tag</u>")],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_literal_equation_marker_edits_preserve_literal_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live literal equation marker {}", unique_suffix()),
        vec![paragraph_child("Literal $E=mc^2$ text")],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_literal_explicit_mention_marker_edits_preserve_literal_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live literal mention marker {}", unique_suffix()),
        vec![paragraph_child("Literal @date(2026-06-14) marker")],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_literal_markdown_inline_marker_edits_preserve_literal_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live literal markdown marker {}", unique_suffix()),
        vec![paragraph_child(
            "Literal **bold** _italic_ ~~strike~~ `code` [link](https://example.com)",
        )],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_paragraph_literal_block_marker_edits_preserve_paragraph_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live literal block marker {}", unique_suffix()),
        vec![paragraph_child("# Literal heading marker")],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_table_width_change_blocks_before_journaled_apply() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live table width guard {}", unique_suffix()),
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
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_table_middle_row_delete_blocks_before_journaled_apply() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live table middle row guard {}", unique_suffix()),
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
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_child_page_link_move_blocks_before_journaled_apply() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let anchor_text = "Anchor before child page.";
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live child link move parent {}", unique_suffix()),
        vec![paragraph_child(anchor_text)],
    );
    let child_title = format!("AFS live child link move child {}", unique_suffix());
    let child = cleanup.create_page(
        &parent.id,
        &child_title,
        vec![paragraph_child("Child page body.")],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_child_page_link_delete_blocks_before_journaled_apply() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let anchor_text = "Anchor before child page delete.";
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live child link delete parent {}", unique_suffix()),
        vec![paragraph_child(anchor_text)],
    );
    let child_title = format!("AFS live child link delete child {}", unique_suffix());
    let child = cleanup.create_page(
        &parent.id,
        &child_title,
        vec![paragraph_child("Child page body.")],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_lazy_virtual_mount_enumerates_children_and_hydrates_on_open() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live lazy root {}", unique_suffix()),
        vec![paragraph_child(
            "Root page body should not materialize during directory listing.",
        )],
    );
    let child = cleanup.create_page(
        &scratch.id,
        &format!("AFS live lazy child {}", unique_suffix()),
        vec![paragraph_child(
            "Lazy child body materialized only on open.",
        )],
    );
    let connector = NotionConnector::new(
        NotionConfig::default().with_root_page_id(RemoteId::new(scratch.id.clone())),
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
            .any(|item| item.filename == "notion"),
        "{root_children:#?}"
    );

    let source_root = source_root_identifier("notion");
    assert_eq!(
        refresh_virtual_fs_children(&mut store, &connector, &fixture.mount_id, &source_root)
            .expect("refresh source root metadata"),
        1
    );
    let source_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &source_root,
    )
    .expect("list source root");
    let scratch_folder = find_virtual_folder(&source_children.children, &scratch.id);
    assert!(
        !content_root
            .join(&scratch_folder.path)
            .join("page.md")
            .exists(),
        "listing the source root must not hydrate the root page body"
    );

    assert_eq!(
        refresh_virtual_fs_children(
            &mut store,
            &connector,
            &fixture.mount_id,
            &scratch_folder.identifier,
        )
        .expect("refresh page children metadata"),
        1
    );
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_drift_preflight_blocks_push_before_overwriting_remote() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live drift {}", unique_suffix()),
        vec![paragraph_child("Base body before local and remote drift.")],
    );
    let connector = NotionConnector::new(NotionConfig::default());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);
    let local_marker = format!("Local pending edit {}", unique_suffix());
    let remote_marker = format!("Remote competing edit {}", unique_suffix());
    fs::write(
        &page_path,
        original.replace("Base body before local and remote drift.", &local_marker),
    )
    .expect("write local drift edit");

    std::thread::sleep(Duration::from_millis(1200));
    append_remote_paragraph(&cleanup.api, &scratch.id, &remote_marker);

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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_dirty_pull_conflict_can_be_resolved_and_pushed() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let base = "Conflict base body before local and remote edits.";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live conflict resolve {}", unique_suffix()),
        vec![paragraph_child(base)],
    );
    let connector = NotionConnector::new(NotionConfig::default());
    let (_fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);
    let local_marker = format!("Local conflict edit {}", unique_suffix());
    let remote_marker = format!("Remote conflict edit {}", unique_suffix());
    let resolved_marker = format!("Resolved conflict edit {}", unique_suffix());

    fs::write(&page_path, original.replace(base, &local_marker))
        .expect("write local conflict edit");

    std::thread::sleep(Duration::from_millis(1200));
    let paragraph_id = first_live_child_block_id(&cleanup.api, &scratch.id);
    update_remote_paragraph(&cleanup.api, &paragraph_id, &remote_marker);

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

#[test]
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_inspect_explains_remote_and_local_drift_without_mutating() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let base = "Inspect base body before drift.";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live inspect {}", unique_suffix()),
        vec![paragraph_child(base)],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_push_log_and_undo_restores_remote_content() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let base = "Undo base body before push.";
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live undo {}", unique_suffix()),
        vec![paragraph_child(base)],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
        },
    )
    .expect("log reverted undo push");
    assert_eq!(reverted_log.entries[0].push_id, push_id);
    assert_eq!(reverted_log.entries[0].status, "reverted");
}

#[test]
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_undo_archive_restores_paragraph_link_without_link_to_page_promotion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let target = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live undo paragraph link target {}", unique_suffix()),
        vec![paragraph_child("Target for paragraph-link undo.")],
    );
    let target_url = notion_object_url(&target.id);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live undo paragraph link source {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "paragraph",
            "paragraph": { "rich_text": [linked_text_part("Linked page", &target_url)] }
        })],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_page_directory_create_pushes_child_page_and_refreshes_parent() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live page-dir parent {}", unique_suffix()),
        vec![paragraph_child("Parent body before child page creation.")],
    );
    let connector = NotionConnector::new(NotionConfig::default());
    let (_fixture, mut store, parent_page_path, _markdown) =
        pull_live_page(&connector, &scratch.id);
    let child_title = format!("AFS live page-dir child {}", unique_suffix());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_virtual_page_directory_delete_archives_remote_child_page() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live virtual delete parent {}", unique_suffix()),
        vec![paragraph_child("Parent body before child page delete.")],
    );
    let child_title = format!("AFS live virtual delete child {}", unique_suffix());
    let child = cleanup.create_page(
        &parent.id,
        &child_title,
        vec![paragraph_child("Child body before delete.")],
    );
    let connector = NotionConnector::new(
        NotionConfig::default().with_root_page_id(RemoteId::new(parent.id.clone())),
    );
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    mount_virtual_workspace(&fixture, &mut store, &parent.id);
    let content_root = fixture.content_root();
    let source_root = source_root_identifier("notion");
    refresh_virtual_fs_children(&mut store, &connector, &fixture.mount_id, &source_root)
        .expect("refresh source root");
    let source_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &source_root,
    )
    .expect("list source root");
    let parent_folder = find_virtual_folder(&source_children.children, &parent.id);
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_virtual_page_directory_rename_updates_remote_title_and_reconciles() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live virtual rename parent {}", unique_suffix()),
        vec![paragraph_child("Parent body before child page rename.")],
    );
    let original_child_title = format!("AFS live virtual rename child {}", unique_suffix());
    let child = cleanup.create_page(
        &parent.id,
        &original_child_title,
        vec![paragraph_child("Child body before rename.")],
    );
    let connector = NotionConnector::new(
        NotionConfig::default().with_root_page_id(RemoteId::new(parent.id.clone())),
    );
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    mount_virtual_workspace(&fixture, &mut store, &parent.id);
    let content_root = fixture.content_root();
    let source_root = source_root_identifier("notion");
    refresh_virtual_fs_children(&mut store, &connector, &fixture.mount_id, &source_root)
        .expect("refresh source root");
    let source_children = virtual_fs_children_with_content_root(
        &store,
        &content_root,
        &fixture.mount_id,
        &source_root,
    )
    .expect("list source root");
    let parent_folder = find_virtual_folder(&source_children.children, &parent.id);
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

    let renamed_child_title = format!("AFS live virtual renamed child {}", unique_suffix());
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
        .get_virtual_mutation(&fixture.mount_id, &format!("rename:{}", child.id))
        .expect("get rename mutation")
        .expect("rename mutation");
    assert_eq!(
        pending.mutation_kind,
        afs_store::VirtualMutationKind::Rename
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_remote_fast_forward_updates_clean_file_and_preserves_pending_file() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live fast forward {}", unique_suffix()),
        vec![paragraph_child("Fast forward base body.")],
    );
    let connector = NotionConnector::new(
        NotionConfig::default().with_root_page_id(RemoteId::new(scratch.id.clone())),
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_scheduled_pull_queues_and_applies_remote_fast_forward() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let parent = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live scheduled pull parent {}", unique_suffix()),
        vec![paragraph_child("Scheduler parent body.")],
    );
    let child = cleanup.create_page(
        &parent.id,
        &format!("AFS live scheduled pull child {}", unique_suffix()),
        vec![paragraph_child("Scheduler child base body.")],
    );
    let connector = NotionConnector::new(
        NotionConfig::default().with_root_page_id(RemoteId::new(parent.id.clone())),
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

    std::thread::sleep(Duration::from_millis(1200));
    let remote_marker = format!("Scheduler remote fast-forward {}", unique_suffix());
    append_remote_paragraph(&cleanup.api, &child.id, &remote_marker);
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_locate_notion_url_returns_markdown_path_and_can_prioritize_hydration() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live locate {}", unique_suffix()),
        vec![paragraph_child(
            "Located page body should hydrate after URL lookup.",
        )],
    );
    let connector = NotionConnector::new(
        NotionConfig::default().with_root_page_id(RemoteId::new(scratch.id.clone())),
    );
    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    mount_virtual_workspace(&fixture, &mut store, &scratch.id);
    let content_root = fixture.content_root();
    let source_root = source_root_identifier("notion");
    refresh_virtual_fs_children(&mut store, &connector, &fixture.mount_id, &source_root)
        .expect("index source root");

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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cyclic_diverse_page_read_noop_preserves_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let target = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS cyclic link target {}", unique_suffix()),
        vec![paragraph_child("Target page for live link checks.")],
    );
    let linked_database = cleanup.create_database(
        &env.parent_page_id,
        &format!("AFS cyclic linked database {}", unique_suffix()),
    );
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS cyclic diverse read {}", unique_suffix()),
        diverse_page_children(&target.id, &linked_database.id),
    );
    cleanup.create_page(
        &source.id,
        &format!("AFS cyclic nested child {}", unique_suffix()),
        vec![paragraph_child(
            "Nested child page for directory projection checks.",
        )],
    );

    let connector = NotionConnector::new(NotionConfig::default());
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
        "target mention [AFS cyclic link target",
        "database mention [AFS cyclic linked database",
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_code_block_with_embedded_fence_edits_round_trip() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live embedded code fence {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "code",
            "code": {
                "rich_text": rich_text_json("Before\n```python\nprint('nested')\n```\nAfter"),
                "language": "markdown",
            }
        })],
    );

    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_code_block_ignores_fence_marker_with_trailing_text() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live code false closer {}", unique_suffix()),
        vec![json!({
            "object": "block",
            "type": "code",
            "code": {
                "rich_text": rich_text_json("Before"),
                "language": "markdown",
            }
        })],
    );

    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cyclic_supported_block_edits_push_and_verify_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let user_id = cleanup.current_user_id();
    let target = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS cyclic supported link target {}", unique_suffix()),
        vec![paragraph_child("Target page for supported edit links.")],
    );
    let linked_database = cleanup.create_database(
        &env.parent_page_id,
        &format!("AFS cyclic supported linked database {}", unique_suffix()),
    );
    let source = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS cyclic supported edits {}", unique_suffix()),
        supported_edit_children(&user_id, &target.id, &linked_database.id),
    );

    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_local_image_media_edit_uploads_and_reconciles_bytes() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live local image {}", unique_suffix()),
        vec![media_child(
            "image",
            "https://www.w3.org/Icons/w3c_home.png",
            "Original local image",
        )],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_local_image_media_edit_with_escaped_caption_uploads_and_reconciles() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live escaped local image {}", unique_suffix()),
        vec![media_child(
            "image",
            "https://www.w3.org/Icons/w3c_home.png",
            "Original escaped image",
        )],
    );
    let connector = NotionConnector::new(NotionConfig::default());
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_local_file_like_media_appends_upload_and_reconcile_local_links() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS live local files {}", unique_suffix()),
        vec![paragraph_child("Base body before local file-like uploads.")],
    );
    let connector = NotionConnector::new(NotionConfig::default());
    let (fixture, mut store, page_path, original) = pull_live_page(&connector, &scratch.id);
    let media_dir = fixture
        .root
        .join(".afs")
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
#[ignore = "requires NOTION_TOKEN and AFS_NOTION_LIVE_PARENT_PAGE; creates and archives scratch Notion content"]
fn live_cyclic_database_rows_mount_edit_create_and_verify_notion() {
    let env = LiveEnv::from_env();
    let api = HttpNotionApi::new(NotionConfig::default());
    let mut cleanup = LiveCleanup::new(api);
    let people_user_id = cleanup.current_user_id();
    let scratch = cleanup.create_page(
        &env.parent_page_id,
        &format!("AFS cyclic database scratch {}", unique_suffix()),
        Vec::new(),
    );
    let related_database = cleanup.create_database(
        &scratch.id,
        &format!("AFS cyclic related rows {}", unique_suffix()),
    );
    let related_data_source_id = related_database
        .data_sources
        .first()
        .expect("related data source")
        .id
        .clone();
    let related_row = cleanup.create_database_row(
        &related_database,
        &format!("AFS cyclic related row {}", unique_suffix()),
        serde_json::Map::new(),
        vec![paragraph_child("Related row target.")],
    );
    let database = cleanup.create_database_with_relation(
        &scratch.id,
        &format!("AFS cyclic rows {}", unique_suffix()),
        &related_data_source_id,
    );
    let existing_row = cleanup.create_database_row(
        &database,
        &format!("AFS cyclic existing row {}", unique_suffix()),
        database_row_properties(
            "Initial row notes",
            "7",
            "Todo",
            "Not started",
            false,
            "https://example.com/afs-db-row",
            &[],
            &[related_row.id.as_str()],
        ),
        vec![paragraph_child("Database row paragraph original.")],
    );

    let fixture = E2eFixture::new();
    let mut store = InMemoryStateStore::new();
    let connector = NotionConnector::new(NotionConfig::default());
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

    let row_path = fixture.nested_markdown_file_containing("AFS cyclic existing row");
    run_pull(&mut store, &connector, &row_path).expect("hydrate live database row");
    let original = fs::read_to_string(&row_path).expect("read hydrated row markdown");
    for expected in [
        "title: \"AFS cyclic existing row",
        "\"Notes\": \"Initial row notes\"",
        "\"Points\": 7",
        "\"Status\": \"Todo\"",
        "\"State\": \"Not started\"",
        "\"Done\": false",
        "\"URL\": \"https://example.com/afs-db-row\"",
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
            "\"URL\": \"https://example.com/afs-db-row\"",
            "\"URL\": \"https://example.com/afs-db-row-updated\"",
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
        "\"URL\": \"https://example.com/afs-db-row-updated\"",
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
        &format!(
            "---\ntitle: AFS cyclic created row\nNotes: \"Created **row** notes and [docs](https://example.com/created-notes)\"\nPoints: 13\nStatus: Todo\nState: Not started\nTags:\n  - Alpha\nDone: false\nDue: \"2026-06-13\"\nURL: https://example.com/afs-created-row\nEmail: cyclic@example.com\nPhone: \"+1 415 555 0199\"\nFiles:\n  - Created file <https://example.com/created.pdf>\nPeople:\n  - \"{}\"\nRelated:\n  - \"{}\"\n---\n# Created row body\n\nCreated from mounted markdown.\n",
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

    let created = render_live_markdown(&connector, &created_row_id, &new_row_path);
    for expected in [
        "title: \"AFS cyclic created row\"",
        "\"Notes\": \"Created **row** notes and [docs](https://example.com/created-notes)\"",
        "\"Points\": 13",
        "\"Status\": \"Todo\"",
        "\"State\": \"Not started\"",
        "\"Tags\":",
        "\"Alpha\"",
        "\"Done\": false",
        "\"URL\": \"https://example.com/afs-created-row\"",
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
            "afs-cli-e2e-push-{}-{unique}-{suffix}",
            std::process::id()
        ));
        let state_root = std::env::temp_dir().join(format!(
            "afs-cli-e2e-push-state-{}-{unique}-{suffix}",
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

fn mount_virtual_workspace(fixture: &E2eFixture, store: &mut InMemoryStateStore, root_id: &str) {
    run_mount(
        store,
        MountOptions {
            mount_id: fixture.mount_id.clone(),
            connector: "notion".to_string(),
            root: fixture.root.clone(),
            remote_root_id: Some(RemoteId::new(root_id.to_string())),
            connection_id: None,
            read_only: false,
            projection: ProjectionMode::LinuxFuse,
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
    let source_root = source_root_identifier("notion");
    refresh_virtual_fs_children(store, connector, &fixture.mount_id, &source_root)
        .expect("refresh virtual source root");
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
    items: &'a [afsd::virtual_fs::VirtualFsItem],
    remote_id: &str,
) -> &'a afsd::virtual_fs::VirtualFsItem {
    items
        .iter()
        .find(|item| {
            item.remote_id.as_deref() == Some(remote_id)
                && item.kind == afsd::virtual_fs::VirtualFsItemKind::Folder
        })
        .unwrap_or_else(|| panic!("missing virtual folder for {remote_id}: {items:#?}"))
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

#[derive(Debug)]
struct LiveEnv {
    parent_page_id: String,
}

impl LiveEnv {
    fn from_env() -> Self {
        std::env::var(TOKEN_ENV).expect("NOTION_TOKEN");
        let parent_page = std::env::var(LIVE_PARENT_ENV)
            .unwrap_or_else(|_| panic!("set {LIVE_PARENT_ENV} to a writable page ID or URL"));
        Self {
            parent_page_id: normalize_notion_id(&parent_page),
        }
    }
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

#[derive(Debug)]
struct MutableNotionApi {
    page: PageDto,
    blocks: Mutex<Vec<BlockDto>>,
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
        Self {
            page: page("page-1", "Initial Idea"),
            blocks: Mutex::new(blocks),
            append_count: Mutex::new(0),
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl NotionApi for MutableNotionApi {
    fn retrieve_page(&self, page_id: &str) -> afs_core::AfsResult<PageDto> {
        if page_id == self.page.id {
            Ok(self.page.clone())
        } else {
            Err(afs_core::AfsError::InvalidState(format!(
                "missing page {page_id}"
            )))
        }
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        _start_cursor: Option<&str>,
    ) -> afs_core::AfsResult<BlockListDto> {
        if block_id == self.page.id {
            Ok(PaginatedListDto {
                results: self.blocks.lock().expect("blocks").clone(),
                next_cursor: None,
                has_more: false,
            })
        } else {
            Ok(PaginatedListDto::default())
        }
    }

    fn search_pages(&self, _start_cursor: Option<&str>) -> afs_core::AfsResult<PageListDto> {
        Ok(PaginatedListDto {
            results: vec![self.page.clone()],
            next_cursor: None,
            has_more: false,
        })
    }

    fn update_block(&self, block_id: &str, body: Value) -> afs_core::AfsResult<BlockDto> {
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
    ) -> afs_core::AfsResult<BlockDto> {
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
    ) -> afs_core::AfsResult<BlockListDto> {
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

    fn delete_block(&self, block_id: &str) -> afs_core::AfsResult<BlockDto> {
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum WriteCall {
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
    Delete {
        block_id: String,
    },
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
            && href.contains(".afs/media/"),
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
    b"<!doctype html><title>AFS upload</title><p>hello</p>\n"
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
