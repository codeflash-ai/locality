use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::mount::{MountOptions, run_mount};
use loc_cli::pull::{run_pull, run_pull_with_state_root};
use loc_cli::status::{StatusOptions, StatusSyncState, run_status};
use locality_core::canonical::render_canonical_markdown;
use locality_core::conflict::{
    CONFLICT_LOCAL_MARKER, CONFLICT_REMOTE_MARKER, CONFLICT_SEPARATOR_MARKER,
    has_unresolved_conflict_markers,
};
use locality_core::freshness::{FreshnessTier, RemoteVersion};
use locality_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use locality_notion::client::NotionApi;
use locality_notion::dto::{
    BlockDto, BlockListDto, DataSourceDto, DataSourcePropertyDto, DataSourceSummaryDto,
    DatabaseDto, PageDto, PageListDto, PagePropertyDto, PaginatedListDto, RichTextBlockDto,
    RichTextDto, SelectOptionDto, SelectPropertySchemaDto, TextRichTextDto, TitleBlockDto,
};
use locality_notion::{NotionConfig, NotionConnector};
use locality_store::{
    EntityRecord, EntityRepository, FreshnessStateRecord, FreshnessStateRepository,
    InMemoryStateStore, MountConfig, MountRepository, ProjectionMode, RemoteObservationRecord,
    RemoteObservationRepository, ShadowRepository,
};
use localityd::virtual_fs::{virtual_fs_content_root, virtual_projection_mount_point};

#[test]
fn pull_mount_root_enumerates_stubs_and_hydrates_root_page() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    let connector = fixture.connector("Roadmap");

    let report = run_pull(&mut store, &connector, &fixture.root).expect("pull root");

    assert!(report.ok);
    assert_eq!(report.enumerated, 4);
    assert_eq!(report.stubbed, 3);
    assert_eq!(report.hydrated, 1);
    assert!(fixture.root_file("roadmap").exists());
    assert!(fixture.child_file("roadmap").exists());
    assert!(fixture.database_schema_file().exists());
    assert!(fixture.row_file().exists());
    assert!(
        !fs::read_to_string(fixture.root_file("roadmap"))
            .expect("root file")
            .contains(locality_core::model::CanonicalDocument::STUB_MARKER)
    );
    assert!(
        fs::read_to_string(fixture.child_file("roadmap"))
            .expect("child file")
            .contains(locality_core::model::CanonicalDocument::STUB_MARKER)
    );
    let schema = fs::read_to_string(fixture.database_schema_file()).expect("schema file");
    assert!(schema.contains("type: notion_database_schema"));
    assert!(schema.contains("\"Status\":"));
    let row = fs::read_to_string(fixture.row_file()).expect("row file");
    assert!(row.contains("\"Status\": \"Todo\""));
    assert!(row.contains(locality_core::model::CanonicalDocument::STUB_MARKER));

    assert!(
        store
            .get_entity(&fixture.mount_id, &fixture.root_page_id)
            .expect("compact root entity lookup")
            .is_none()
    );
    let root_entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get root entity")
        .expect("root entity");
    assert_eq!(root_entity.hydration, HydrationState::Hydrated);
    assert!(
        store
            .load_shadow(&fixture.mount_id, &fixture.canonical_root_page_id)
            .is_ok()
    );
}

#[test]
fn pull_fast_forward_refreshes_stale_remote_observation_for_status() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    run_pull(&mut store, &fixture.connector("Roadmap"), &fixture.root).expect("initial pull");
    store
        .save_remote_observation(
            RemoteObservationRecord::new(
                fixture.mount_id.clone(),
                fixture.canonical_root_page_id.clone(),
                EntityKind::Page,
                "Roadmap",
                "roadmap/page.md",
                "unix_ms:1",
            )
            .with_remote_version(RemoteVersion::new("2026-06-09T00:00:00.000Z")),
        )
        .expect("save stale observation");
    store
        .save_freshness_state(
            FreshnessStateRecord::new(
                fixture.mount_id.clone(),
                fixture.canonical_root_page_id.clone(),
                FreshnessTier::Hot,
            )
            .checked_at("unix_ms:1"),
        )
        .expect("save freshness state");

    run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body.", "2026-06-11T00:00:00.000Z"),
        fixture.root_file("roadmap"),
    )
    .expect("fast-forward pull");

    let status = run_status(
        &store,
        StatusOptions {
            path: Some(fixture.root_file("roadmap")),
            ..StatusOptions::default()
        },
    )
    .expect("status after fast-forward pull");
    let entry = &status.mounts[0].entries[0];
    assert_eq!(entry.sync_state, StatusSyncState::AllSynced);
    assert_eq!(status.summary.remote_update_available, 0);
    assert_eq!(status.summary.review_needed, 0);
    assert!(!entry.remote.changed);
    assert_eq!(
        entry.remote.remote_tree_version.as_deref(),
        Some("2026-06-11T00:00:00.000Z")
    );
}

#[test]
fn pull_virtual_mount_writes_content_and_schema_to_daemon_cache() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::LinuxFuse);
    let connector = fixture.connector("Roadmap");

    let report = run_pull_with_state_root(&mut store, &connector, &fixture.root, Some(&state_root))
        .expect("pull virtual root");

    assert!(report.ok);
    assert_eq!(report.stubbed, 0);
    assert_eq!(report.hydrated, 1);
    assert!(!fixture.root_file("roadmap").exists());
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    assert!(content_root.join("roadmap/page.md").exists());
    assert!(
        content_root
            .join("roadmap")
            .join("tasks")
            .join("_schema.yaml")
            .exists()
    );

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_virtual_file_target_does_not_stat_projection_path_as_directory() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::LinuxFuse);
    let connector = fixture.connector("Roadmap");
    run_pull_with_state_root(&mut store, &connector, &fixture.root, Some(&state_root))
        .expect("pull virtual root");

    fs::create_dir_all(fixture.root_file("roadmap")).expect("sentinel directory at VFS file path");

    let report = run_pull_with_state_root(
        &mut store,
        &connector,
        fixture.root_file("roadmap"),
        Some(&state_root),
    )
    .expect("pull virtual file target");

    assert!(report.ok);
    assert_eq!(report.enumerated, 0);
    assert_eq!(report.hydrated, 1);
    assert_eq!(report.stubbed, 0);

    let _ = fs::remove_dir_all(state_root);
    let _ = fs::remove_dir_all(&fixture.root);
}

#[cfg(target_os = "macos")]
#[test]
fn pull_macos_file_provider_accepts_system_assigned_connector_roots() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-macos-state");
    let home = std::env::var_os("HOME").map(PathBuf::from).expect("home");
    let mount_root = home
        .join("Library")
        .join("CloudStorage")
        .join("Locality")
        .join("notion");
    let system_assigned_root = home
        .join("Library")
        .join("CloudStorage")
        .join("Locality")
        .join("notion");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &mount_root)
                .with_remote_root_id(fixture.root_page_id.clone())
                .projection(ProjectionMode::MacosFileProvider),
        )
        .expect("save macos file provider mount");
    let connector = fixture.connector("Roadmap");

    run_pull_with_state_root(
        &mut store,
        &connector,
        &system_assigned_root,
        Some(&state_root),
    )
    .expect("pull through system-assigned file provider connector root");
    let report = run_pull_with_state_root(
        &mut store,
        &connector,
        system_assigned_root.join("roadmap").join("page.md"),
        Some(&state_root),
    )
    .expect("pull through system-assigned file provider file");

    assert!(report.ok);
    assert_eq!(report.hydrated, 1);
    assert!(
        virtual_fs_content_root(&state_root, &fixture.mount_id)
            .join("roadmap")
            .join("page.md")
            .exists()
    );

    let _ = fs::remove_dir_all(state_root);
}

#[cfg(target_os = "macos")]
#[test]
fn pull_macos_file_provider_reconciles_missed_visible_edit_before_refresh() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-macos-missed-edit-state");
    let mount_root = fixture.root.join("notion");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &mount_root)
                .with_remote_root_id(fixture.root_page_id.clone())
                .projection(ProjectionMode::MacosFileProvider),
        )
        .expect("save macos file provider mount");
    let connector = fixture.connector("Roadmap");
    run_pull_with_state_root(&mut store, &connector, &mount_root, Some(&state_root))
        .expect("initial pull");

    let content_path = virtual_fs_content_root(&state_root, &fixture.mount_id)
        .join("roadmap")
        .join("page.md");
    let visible_path = mount_root.join("roadmap").join("page.md");
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::copy(&content_path, &visible_path).expect("seed visible replica");
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(
        &visible_path,
        "---\nloc:\n  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\nLocal visible edit.\n",
    )
    .expect("missed visible edit");

    let report = run_pull_with_state_root(&mut store, &connector, &visible_path, Some(&state_root))
        .expect("pull visible file");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
    let visible = fs::read_to_string(&visible_path).expect("read visible replica");
    assert!(visible.contains("Local visible edit."));
    let cached = fs::read_to_string(&content_path).expect("read daemon cache");
    assert!(cached.contains("Local visible edit."));
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get root entity")
        .expect("root entity");
    assert_eq!(entity.hydration, HydrationState::Dirty);

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_visible_provider_reconciles_missed_visible_edit_before_refresh() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-visible-missed-edit-state");
    let mount_root = fixture.root.join("notion");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &mount_root)
                .with_remote_root_id(fixture.root_page_id.clone())
                .projection(ProjectionMode::MacosFileProvider),
        )
        .expect("save visible provider mount");
    let connector = fixture.connector("Roadmap");
    run_pull_with_state_root(&mut store, &connector, &mount_root, Some(&state_root))
        .expect("initial pull");

    let content_path = virtual_fs_content_root(&state_root, &fixture.mount_id)
        .join("roadmap")
        .join("page.md");
    let visible_path = mount_root.join("roadmap").join("page.md");
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::copy(&content_path, &visible_path).expect("seed visible replica");
    fs::write(
        &visible_path,
        "---\nloc:\n  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\nLocal visible edit.\n",
    )
    .expect("missed visible edit");

    let report = run_pull_with_state_root(&mut store, &connector, &visible_path, Some(&state_root))
        .expect("pull visible file");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
    assert!(report.conflicts.is_empty());
    let visible = fs::read_to_string(&visible_path).expect("read visible replica");
    assert!(visible.contains("Local visible edit."), "{visible}");
    assert!(!has_unresolved_conflict_markers(&visible), "{visible}");
    let cached = fs::read_to_string(&content_path).expect("read daemon cache");
    assert!(cached.contains("Local visible edit."), "{cached}");
    assert!(!has_unresolved_conflict_markers(&cached), "{cached}");
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get root entity")
        .expect("root entity");
    assert_eq!(entity.hydration, HydrationState::Dirty);

    let _ = fs::remove_dir_all(state_root);
}

#[cfg(target_os = "macos")]
#[test]
fn pull_macos_file_provider_refreshes_clean_visible_replica_after_conflict_pull() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-macos-conflict-refresh-state");
    let mount_root = fixture.root.join("notion");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &mount_root)
                .with_remote_root_id(fixture.root_page_id.clone())
                .projection(ProjectionMode::MacosFileProvider),
        )
        .expect("save macos file provider mount");
    run_pull_with_state_root(
        &mut store,
        &fixture.connector("Roadmap"),
        &mount_root,
        Some(&state_root),
    )
    .expect("initial pull");

    let content_path = virtual_fs_content_root(&state_root, &fixture.mount_id)
        .join("roadmap")
        .join("page.md");
    let visible_path = mount_root.join("roadmap").join("page.md");
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::copy(&content_path, &visible_path).expect("seed clean visible replica");
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(
        &content_path,
        render_canonical_markdown(&CanonicalDocument::new(
            root_frontmatter(&fixture.canonical_root_page_id, "2026-06-10T00:00:00.000Z"),
            "# Roadmap\n\nLocal cache edit.\n".to_string(),
        )),
    )
    .expect("write dirty daemon cache");
    let mut entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get root entity")
        .expect("root entity");
    entity.hydration = HydrationState::Dirty;
    store.save_entity(entity).expect("mark cache dirty");

    let report = run_pull_with_state_root(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body.", "2026-06-11T00:00:00.000Z"),
        &visible_path,
        Some(&state_root),
    )
    .expect("pull visible file");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
    assert_eq!(report.conflicts.len(), 1);
    let cached = fs::read_to_string(&content_path).expect("read daemon cache");
    assert!(cached.contains("Local cache edit."));
    assert!(cached.contains("Remote body."));
    assert!(cached.contains(CONFLICT_LOCAL_MARKER));
    let visible = fs::read_to_string(&visible_path).expect("read visible replica");
    assert_eq!(visible, cached);

    let _ = fs::remove_dir_all(state_root);
}

#[cfg(target_os = "macos")]
#[test]
fn pull_macos_file_provider_writes_conflict_markers_to_missed_visible_edit() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-macos-visible-conflict-state");
    let mount_root = fixture.root.join("notion");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &mount_root)
                .with_remote_root_id(fixture.root_page_id.clone())
                .projection(ProjectionMode::MacosFileProvider),
        )
        .expect("save macos file provider mount");
    run_pull_with_state_root(
        &mut store,
        &fixture.connector("Roadmap"),
        &mount_root,
        Some(&state_root),
    )
    .expect("initial pull");

    let content_path = virtual_fs_content_root(&state_root, &fixture.mount_id)
        .join("roadmap")
        .join("page.md");
    let visible_path = mount_root.join("roadmap").join("page.md");
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::copy(&content_path, &visible_path).expect("seed visible replica");
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(
        &visible_path,
        "---\nloc:\n  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\nLocal visible edit.\n",
    )
    .expect("missed visible edit");

    let report = run_pull_with_state_root(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body.", "2026-06-11T00:00:00.000Z"),
        &visible_path,
        Some(&state_root),
    )
    .expect("pull visible file");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
    assert_eq!(report.conflicts.len(), 1);
    let cached = fs::read_to_string(&content_path).expect("read daemon cache");
    assert!(cached.contains("Local visible edit."));
    assert!(cached.contains("Remote body."));
    assert!(cached.contains(CONFLICT_LOCAL_MARKER));
    assert!(cached.contains(CONFLICT_SEPARATOR_MARKER));
    assert!(cached.contains(CONFLICT_REMOTE_MARKER));
    assert!(has_unresolved_conflict_markers(&cached));
    let visible = fs::read_to_string(&visible_path).expect("read visible replica");
    assert_eq!(visible, cached);
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get root entity")
        .expect("root entity");
    assert_eq!(entity.hydration, HydrationState::Conflicted);

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_visible_provider_writes_conflict_markers_to_missed_visible_edit_after_remote_change() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-visible-conflict-state");
    let mount_root = fixture.root.join("notion");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &mount_root)
                .with_remote_root_id(fixture.root_page_id.clone())
                .projection(ProjectionMode::MacosFileProvider),
        )
        .expect("save visible provider mount");
    run_pull_with_state_root(
        &mut store,
        &fixture.connector("Roadmap"),
        &mount_root,
        Some(&state_root),
    )
    .expect("initial pull");

    let content_path = virtual_fs_content_root(&state_root, &fixture.mount_id)
        .join("roadmap")
        .join("page.md");
    let visible_path = mount_root.join("roadmap").join("page.md");
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::copy(&content_path, &visible_path).expect("seed visible replica");
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(
        &visible_path,
        "---\nloc:\n  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\nLocal visible edit.\n",
    )
    .expect("missed visible edit");

    let report = run_pull_with_state_root(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body.", "2026-06-11T00:00:00.000Z"),
        &visible_path,
        Some(&state_root),
    )
    .expect("pull visible file");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
    assert_eq!(report.conflicts.len(), 1);
    let cached = fs::read_to_string(&content_path).expect("read daemon cache");
    assert!(cached.contains("Local visible edit."), "{cached}");
    assert!(cached.contains("Remote body."), "{cached}");
    assert!(cached.contains(CONFLICT_LOCAL_MARKER), "{cached}");
    assert!(cached.contains(CONFLICT_SEPARATOR_MARKER), "{cached}");
    assert!(cached.contains(CONFLICT_REMOTE_MARKER), "{cached}");
    assert!(has_unresolved_conflict_markers(&cached), "{cached}");
    let visible = fs::read_to_string(&visible_path).expect("read visible replica");
    assert_eq!(visible, cached);
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get root entity")
        .expect("root entity");
    assert_eq!(entity.hydration, HydrationState::Conflicted);

    let _ = fs::remove_dir_all(state_root);
}

#[cfg(target_os = "macos")]
#[test]
fn pull_macos_file_provider_preserves_older_visible_replica_after_cache_fast_forward() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-macos-stale-visible-state");
    let mount_root = fixture.root.join("notion");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &mount_root)
                .with_remote_root_id(fixture.root_page_id.clone())
                .projection(ProjectionMode::MacosFileProvider),
        )
        .expect("save macos file provider mount");
    run_pull_with_state_root(
        &mut store,
        &fixture.connector("Roadmap"),
        &mount_root,
        Some(&state_root),
    )
    .expect("initial pull");

    let content_path = virtual_fs_content_root(&state_root, &fixture.mount_id)
        .join("roadmap")
        .join("page.md");
    let visible_path = mount_root.join("roadmap").join("page.md");
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::copy(&content_path, &visible_path).expect("seed stale visible replica");
    std::thread::sleep(std::time::Duration::from_millis(20));
    seed_remote_fast_forward_cache(
        &mut store,
        &content_path,
        &fixture.mount_id,
        &fixture.canonical_root_page_id,
        "Remote body.",
    );

    let report = run_pull_with_state_root(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body.", "2026-06-11T00:00:00.000Z"),
        &visible_path,
        Some(&state_root),
    )
    .expect("pull visible file");

    assert!(report.ok);
    assert_eq!(report.hydrated, 1);
    assert_eq!(report.skipped_dirty, 0);
    let visible = fs::read_to_string(&visible_path).expect("read visible replica");
    assert!(visible.contains("Root body."));
    assert!(!visible.contains("Remote body."));
    let cached = fs::read_to_string(&content_path).expect("read daemon cache");
    assert!(cached.contains("Remote body."));
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get root entity")
        .expect("root entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);

    let _ = fs::remove_dir_all(state_root);
}

#[cfg(target_os = "macos")]
#[test]
fn pull_macos_file_provider_preserves_older_visible_edit_after_cache_fast_forward() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-macos-older-visible-edit-state");
    let mount_root = fixture.root.join("notion");
    let mut store = InMemoryStateStore::new();
    store
        .save_mount(
            MountConfig::new(fixture.mount_id.clone(), "notion", &mount_root)
                .with_remote_root_id(fixture.root_page_id.clone())
                .projection(ProjectionMode::MacosFileProvider),
        )
        .expect("save macos file provider mount");
    run_pull_with_state_root(
        &mut store,
        &fixture.connector("Roadmap"),
        &mount_root,
        Some(&state_root),
    )
    .expect("initial pull");

    let content_path = virtual_fs_content_root(&state_root, &fixture.mount_id)
        .join("roadmap")
        .join("page.md");
    let visible_path = mount_root.join("roadmap").join("page.md");
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::write(
        &visible_path,
        "---\nloc:\n  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\nLocal visible edit.\n",
    )
    .expect("missed visible edit");
    std::thread::sleep(std::time::Duration::from_millis(20));
    seed_remote_fast_forward_cache(
        &mut store,
        &content_path,
        &fixture.mount_id,
        &fixture.canonical_root_page_id,
        "Remote body.",
    );

    let report = run_pull_with_state_root(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body.", "2026-06-11T00:00:00.000Z"),
        &visible_path,
        Some(&state_root),
    )
    .expect("pull visible file");

    assert!(report.ok);
    assert_eq!(report.hydrated, 1);
    assert_eq!(report.skipped_dirty, 0);
    let visible = fs::read_to_string(&visible_path).expect("read visible replica");
    assert!(visible.contains("Local visible edit."));
    assert!(!visible.contains("Remote body."));
    let cached = fs::read_to_string(&content_path).expect("read daemon cache");
    assert!(cached.contains("Remote body."));
    assert!(!cached.contains("Local visible edit."));
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get root entity")
        .expect("root entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_windows_cloud_files_refreshes_clean_visible_replica_after_pull() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("afs-cli-pull-windows-visible-refresh-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::WindowsCloudFiles);
    run_pull_with_state_root(
        &mut store,
        &fixture.connector("Roadmap"),
        &fixture.root,
        Some(&state_root),
    )
    .expect("initial pull");

    let content_path = virtual_fs_content_root(&state_root, &fixture.mount_id)
        .join("roadmap")
        .join("page.md");
    let visible_path = fixture.mount_point_root().join("roadmap").join("page.md");
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::copy(&content_path, &visible_path).expect("seed clean visible replica");
    let stale_visible = fs::read_to_string(&visible_path).expect("read stale visible replica");
    assert!(stale_visible.contains("Root body."));

    let report = run_pull_with_state_root(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body.", "2026-06-11T00:00:00.000Z"),
        &visible_path,
        Some(&state_root),
    )
    .expect("pull visible Windows Cloud Files path");

    assert!(report.ok);
    assert_eq!(report.hydrated, 1);
    assert_eq!(report.skipped_dirty, 0);
    let cached = fs::read_to_string(&content_path).expect("read daemon cache");
    assert!(cached.contains("Remote body."));
    let visible = fs::read_to_string(&visible_path).expect("read visible replica");
    assert_eq!(visible, cached);
    assert!(visible.contains("Remote body."));
    assert!(!visible.contains("Root body."));

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_windows_cloud_files_refreshes_visible_empty_conflict_after_clean_merge() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("afs-cli-pull-windows-empty-conflict-refresh-state");
    let mut store = InMemoryStateStore::new();
    let version = "2026-06-11T00:00:00.000Z";
    fixture.mount_with_projection(&mut store, ProjectionMode::WindowsCloudFiles);
    run_pull_with_state_root(
        &mut store,
        &fixture.connector_with("Roadmap", "Shared body.", version),
        &fixture.root,
        Some(&state_root),
    )
    .expect("initial pull");

    let content_path = virtual_fs_content_root(&state_root, &fixture.mount_id)
        .join("roadmap")
        .join("page.md");
    let visible_path = fixture.mount_point_root().join("roadmap").join("page.md");
    fs::create_dir_all(visible_path.parent().expect("visible parent"))
        .expect("create visible parent");
    fs::copy(&content_path, &visible_path).expect("seed visible replica");
    let clean = fs::read_to_string(&visible_path)
        .expect("read clean visible replica")
        .replace(version, "2026-06-10T00:00:00.000Z");
    fs::write(&visible_path, &clean).expect("write visible with stale sync metadata");
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(
        &visible_path,
        format!(
            "{clean}{CONFLICT_LOCAL_MARKER}\n{CONFLICT_SEPARATOR_MARKER}\n{CONFLICT_REMOTE_MARKER}\n"
        ),
    )
    .expect("write visible empty conflict");

    let report = run_pull_with_state_root(
        &mut store,
        &fixture.connector_with("Roadmap", "Shared body.", version),
        &visible_path,
        Some(&state_root),
    )
    .expect("pull visible empty conflict");

    assert!(report.ok);
    assert_eq!(report.conflicts.len(), 0);
    let cached = fs::read_to_string(&content_path).expect("read daemon cache");
    let visible = fs::read_to_string(&visible_path).expect("read visible replica");
    assert_eq!(visible, cached);
    assert_eq!(visible, clean);
    assert!(!has_unresolved_conflict_markers(&visible), "{visible}");

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_virtual_mount_accepts_mount_point_directory_as_root_target() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::LinuxFuse);
    let connector = fixture.connector("Roadmap");

    let report = run_pull_with_state_root(
        &mut store,
        &connector,
        fixture.mount_point_root(),
        Some(&state_root),
    )
    .expect("pull virtual mount point root");

    assert!(report.ok);
    assert_eq!(report.enumerated, 4);
    assert_eq!(report.hydrated, 1);
    assert_eq!(report.stubbed, 0);
    assert!(
        virtual_fs_content_root(&state_root, &fixture.mount_id)
            .join("roadmap/page.md")
            .exists()
    );

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_virtual_file_accepts_mount_point_directory_target() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::LinuxFuse);
    let connector = fixture.connector("Roadmap");
    run_pull_with_state_root(&mut store, &connector, &fixture.root, Some(&state_root))
        .expect("pull virtual root");

    let report = run_pull_with_state_root(
        &mut store,
        &connector,
        fixture.mount_point_child_file("roadmap"),
        Some(&state_root),
    )
    .expect("pull virtual mount point file target");

    assert!(report.ok);
    assert_eq!(report.enumerated, 0);
    assert_eq!(report.hydrated, 1);
    assert_eq!(report.stubbed, 0);
    assert!(
        virtual_fs_content_root(&state_root, &fixture.mount_id)
            .join("roadmap/design-notes/page.md")
            .exists()
    );

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_virtual_page_directory_recursively_hydrates_child_pages() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::LinuxFuse);
    let connector = fixture.connector_with_nested_child("Roadmap");
    run_pull_with_state_root(&mut store, &connector, &fixture.root, Some(&state_root))
        .expect("pull virtual root");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);

    let report = run_pull_with_state_root(
        &mut store,
        &connector,
        fixture
            .mount_point_root()
            .join("roadmap")
            .join("design-notes"),
        Some(&state_root),
    )
    .expect("pull virtual page directory");

    assert!(report.ok);
    assert_eq!(report.enumerated, 1);
    assert_eq!(report.hydrated, 2);
    assert_eq!(report.stubbed, 0);
    let target = store
        .find_entity_by_path(
            &fixture.mount_id,
            &PathBuf::from("roadmap/design-notes/page.md"),
        )
        .expect("find target")
        .expect("target entity");
    assert_eq!(target.hydration, HydrationState::Hydrated);
    let nested = store
        .find_entity_by_path(
            &fixture.mount_id,
            &PathBuf::from("roadmap/design-notes/appendix/page.md"),
        )
        .expect("find nested child")
        .expect("nested child entity");
    assert_eq!(nested.hydration, HydrationState::Hydrated);
    assert!(
        content_root
            .join("roadmap")
            .join("design-notes")
            .join("page.md")
            .exists()
    );
    assert!(
        content_root
            .join("roadmap")
            .join("design-notes")
            .join("page.md")
            .exists()
    );
    assert!(
        fs::read_to_string(
            content_root
                .join("roadmap")
                .join("design-notes")
                .join("page.md"),
        )
        .expect("read target page")
        .contains("Child body.")
    );
    assert!(
        content_root
            .join("roadmap")
            .join("design-notes")
            .join("appendix")
            .join("page.md")
            .exists()
    );

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_file_provider_page_directory_materializes_visible_child_pages() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-visible-directory-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::WindowsCloudFiles);
    let connector = fixture.connector_with_nested_child("Roadmap");
    run_pull_with_state_root(&mut store, &connector, &fixture.root, Some(&state_root))
        .expect("pull virtual root");

    let report = run_pull_with_state_root(
        &mut store,
        &connector,
        fixture
            .mount_point_root()
            .join("roadmap")
            .join("design-notes"),
        Some(&state_root),
    )
    .expect("pull visible page directory");

    assert!(report.ok);
    assert_eq!(report.hydrated, 2);
    let visible_target = fixture
        .mount_point_root()
        .join("roadmap")
        .join("design-notes")
        .join("page.md");
    assert!(visible_target.exists());
    assert!(
        fs::read_to_string(&visible_target)
            .expect("read visible target")
            .contains("Child body.")
    );
    let visible_child = fixture
        .mount_point_root()
        .join("roadmap")
        .join("design-notes")
        .join("appendix")
        .join("page.md");
    assert!(visible_child.exists());
    assert!(
        fs::read_to_string(&visible_child)
            .expect("read visible child")
            .contains("Nested child body.")
    );

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_plain_files_page_directory_hydrates_target_and_descendants() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    let connector = fixture.connector_with_nested_child("Roadmap");
    run_pull(&mut store, &connector, &fixture.root).expect("pull plain-files root");

    let target_page_path = fixture
        .root
        .join("roadmap")
        .join("design-notes")
        .join("page.md");
    let initial_target = fs::read_to_string(&target_page_path).expect("read initial target stub");
    assert!(
        initial_target.contains(CanonicalDocument::STUB_MARKER),
        "{initial_target}"
    );

    let report = run_pull(
        &mut store,
        &connector,
        fixture.root.join("roadmap").join("design-notes"),
    )
    .expect("pull plain-files page directory");

    assert!(report.ok);
    assert_eq!(report.enumerated, 1);
    assert_eq!(report.hydrated, 2);

    let target = store
        .find_entity_by_path(
            &fixture.mount_id,
            &PathBuf::from("roadmap/design-notes/page.md"),
        )
        .expect("find target")
        .expect("target entity");
    assert_eq!(target.hydration, HydrationState::Hydrated);
    let nested = store
        .find_entity_by_path(
            &fixture.mount_id,
            &PathBuf::from("roadmap/design-notes/appendix/page.md"),
        )
        .expect("find nested")
        .expect("nested entity");
    assert_eq!(nested.hydration, HydrationState::Hydrated);
    assert!(
        fs::read_to_string(&target_page_path)
            .expect("read hydrated target")
            .contains("Child body.")
    );
    assert!(
        fs::read_to_string(
            fixture
                .root
                .join("roadmap")
                .join("design-notes")
                .join("appendix")
                .join("page.md"),
        )
        .expect("read hydrated appendix")
        .contains("Nested child body.")
    );
}

#[test]
fn pull_virtual_file_conflict_reports_mount_point_directory_path() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::LinuxFuse);
    let connector = fixture.connector("Roadmap");
    run_pull_with_state_root(&mut store, &connector, &fixture.root, Some(&state_root))
        .expect("initial pull");

    let content_path = virtual_fs_content_root(&state_root, &fixture.mount_id)
        .join("roadmap")
        .join("page.md");
    fs::write(
        &content_path,
        render_canonical_markdown(&CanonicalDocument::new(
            root_frontmatter(&fixture.canonical_root_page_id, "2026-06-10T00:00:00.000Z"),
            "Local conflict body.".to_string(),
        )),
    )
    .expect("write dirty daemon cache");
    let mut entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get root entity")
        .expect("root entity");
    entity.hydration = HydrationState::Dirty;
    store.save_entity(entity).expect("mark cache dirty");

    let target = fixture.mount_point_root().join("roadmap").join("page.md");
    let report = run_pull_with_state_root(
        &mut store,
        &fixture.connector_with(
            "Roadmap",
            "Remote conflict body.",
            "2026-06-11T00:00:00.000Z",
        ),
        &target,
        Some(&state_root),
    )
    .expect("pull conflicted virtual file");

    assert!(!report.ok);
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].path, target.display().to_string());
    let contents = fs::read_to_string(&content_path).expect("read conflict cache");
    assert!(contents.contains("Local conflict body."));
    assert!(contents.contains("Remote conflict body."));
    assert!(contents.contains(CONFLICT_LOCAL_MARKER));

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_virtual_database_directory_hydrates_rows_when_under_limit() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::LinuxFuse);
    let connector = fixture.connector("Roadmap");
    run_pull_with_state_root(&mut store, &connector, &fixture.root, Some(&state_root))
        .expect("pull virtual root");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);
    assert!(content_root.join("roadmap").join("tasks").is_dir());

    let report = run_pull_with_state_root(
        &mut store,
        &connector,
        fixture.mount_point_database_dir(),
        Some(&state_root),
    )
    .expect("pull virtual database directory");

    assert!(report.ok);
    assert_eq!(report.enumerated, 1);
    assert_eq!(report.hydrated, 1);
    assert_eq!(report.stubbed, 0);
    assert!(
        content_root
            .join("roadmap")
            .join("tasks")
            .join("_schema.yaml")
            .exists()
    );
    let row = store
        .find_entity_by_path(
            &fixture.mount_id,
            &PathBuf::from("roadmap/tasks/fix-login-bug/page.md"),
        )
        .expect("find row")
        .expect("row entity");
    assert_eq!(row.kind, EntityKind::Page);
    assert_eq!(row.hydration, HydrationState::Hydrated);
    assert!(
        content_root
            .join("roadmap")
            .join("tasks")
            .join("fix-login-bug")
            .join("page.md")
            .exists()
    );

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_virtual_database_directory_leaves_rows_online_only_when_over_limit() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::LinuxFuse);
    let connector = fixture.connector_with_row_count("Roadmap", 6);
    run_pull_with_state_root(&mut store, &connector, &fixture.root, Some(&state_root))
        .expect("pull virtual root");
    let content_root = virtual_fs_content_root(&state_root, &fixture.mount_id);

    let report = run_pull_with_state_root(
        &mut store,
        &connector,
        fixture.mount_point_database_dir(),
        Some(&state_root),
    )
    .expect("pull virtual database directory");

    assert!(report.ok);
    assert_eq!(report.enumerated, 6);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.stubbed, 0);
    assert!(
        !content_root
            .join("roadmap")
            .join("tasks")
            .join("fix-login-bug")
            .join("page.md")
            .exists()
    );

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_virtual_database_directory_keeps_dirty_row_at_local_path() {
    let fixture = PullFixture::new();
    let state_root = unique_temp_path("loc-cli-pull-state");
    let mut store = InMemoryStateStore::new();
    fixture.mount_with_projection(&mut store, ProjectionMode::LinuxFuse);
    let connector = fixture.connector("Roadmap");
    run_pull_with_state_root(&mut store, &connector, &fixture.root, Some(&state_root))
        .expect("pull virtual root");
    let row_id = store
        .find_entity_by_path(
            &fixture.mount_id,
            &PathBuf::from("roadmap/tasks/fix-login-bug/page.md"),
        )
        .expect("find original row")
        .expect("original row")
        .remote_id;
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                row_id.clone(),
                EntityKind::Page,
                "Fix login bug",
                "local-dirty-row/page.md",
            )
            .with_hydration(HydrationState::Dirty)
            .with_content_hash("local-dirty-hash"),
        )
        .expect("save dirty row");

    let report = run_pull_with_state_root(
        &mut store,
        &connector,
        fixture.mount_point_database_dir(),
        Some(&state_root),
    )
    .expect("pull virtual database directory");

    assert!(report.ok);
    assert_eq!(report.enumerated, 1);
    let row = store
        .get_entity(&fixture.mount_id, &row_id)
        .expect("get row")
        .expect("row");
    assert_eq!(row.path, PathBuf::from("local-dirty-row/page.md"));
    assert_eq!(row.hydration, HydrationState::Dirty);
    assert_eq!(row.content_hash.as_deref(), Some("local-dirty-hash"));

    let _ = fs::remove_dir_all(state_root);
}

#[test]
fn pull_file_skips_dirty_hydrated_file() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    let connector = fixture.connector("Roadmap");
    run_pull(&mut store, &connector, &fixture.root).expect("initial pull");
    fs::write(fixture.root_file("roadmap"), "---\nloc:\n  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\nLocal edit.\n")
        .expect("dirty write");

    let report =
        run_pull(&mut store, &connector, fixture.root_file("roadmap")).expect("pull dirty file");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
    assert!(report.conflicts.is_empty());
}

#[test]
fn pull_mount_root_renames_existing_projection_when_remote_title_changes() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);

    run_pull(&mut store, &fixture.connector("Roadmap"), &fixture.root).expect("initial pull");

    assert!(fixture.root_file("roadmap").exists());
    assert!(fixture.child_file("roadmap").exists());

    let report = run_pull(&mut store, &fixture.connector("Strategy"), &fixture.root)
        .expect("pull renamed root");

    assert!(report.ok);
    assert!(fixture.root_file("strategy").exists());
    assert!(fixture.child_file("strategy").exists());
    assert!(!fixture.root_file("roadmap").exists());
    assert!(!fixture.child_file("roadmap").exists());

    let root_entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get root entity")
        .expect("root entity");
    assert_eq!(root_entity.path, PathBuf::from("strategy/page.md"));
}

#[test]
fn pull_mount_root_renames_existing_child_when_duplicate_title_appears() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);

    run_pull(&mut store, &fixture.connector("Roadmap"), &fixture.root).expect("initial pull");
    assert!(fixture.child_file("roadmap").exists());

    let report = run_pull(
        &mut store,
        &fixture.connector_with_duplicate_child("Roadmap"),
        &fixture.root,
    )
    .expect("pull duplicate child");

    assert!(report.ok);
    assert!(fixture.colliding_child_file("roadmap", "bbbbbb").exists());
    assert!(fixture.colliding_child_file("roadmap", "ffffff").exists());
    assert!(!fixture.child_file("roadmap").exists());

    let existing_child = store
        .get_entity(
            &fixture.mount_id,
            &RemoteId::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        )
        .expect("get existing child entity")
        .expect("existing child entity");
    assert_eq!(
        existing_child.path,
        PathBuf::from("roadmap/design-notes bbbbbb/page.md")
    );
}

#[test]
fn pull_mount_root_renames_existing_database_row_when_duplicate_title_appears() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);

    run_pull(&mut store, &fixture.connector("Roadmap"), &fixture.root).expect("initial pull");
    assert!(fixture.row_file().exists());

    let report = run_pull(
        &mut store,
        &fixture.connector_with_duplicate_row("Roadmap"),
        &fixture.root,
    )
    .expect("pull duplicate row");

    assert!(report.ok);
    assert!(fixture.colliding_row_file("eeeeee").exists());
    assert!(fixture.colliding_row_file("ffffff").exists());
    assert!(!fixture.row_file().exists());

    let existing_row = store
        .get_entity(
            &fixture.mount_id,
            &RemoteId::new("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
        )
        .expect("get existing row entity")
        .expect("existing row entity");
    assert_eq!(
        existing_row.path,
        PathBuf::from("roadmap/tasks/fix-login-bug eeeeee/page.md")
    );
}

#[test]
fn pull_mount_root_preserves_shadow_remote_timestamp_for_non_rehydrated_pages() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);

    run_pull(&mut store, &fixture.connector("Roadmap"), &fixture.root).expect("initial pull");
    run_pull(
        &mut store,
        &fixture.connector("Roadmap"),
        fixture.child_file("roadmap"),
    )
    .expect("hydrate child");

    let child_entity = store
        .get_entity(
            &fixture.mount_id,
            &RemoteId::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        )
        .expect("get child entity")
        .expect("child entity");
    assert_eq!(
        child_entity.remote_edited_at.as_deref(),
        Some("2026-06-10T00:00:00.000Z")
    );

    run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Root body.", "2026-06-11T00:00:00.000Z"),
        &fixture.root,
    )
    .expect("refresh root");

    let child_entity = store
        .get_entity(
            &fixture.mount_id,
            &RemoteId::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        )
        .expect("get child entity")
        .expect("child entity");
    assert_eq!(
        child_entity.remote_edited_at.as_deref(),
        Some("2026-06-10T00:00:00.000Z")
    );
}

#[test]
fn pull_file_removes_clean_stub_when_remote_is_not_found() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    run_pull(&mut store, &fixture.connector("Roadmap"), &fixture.root).expect("initial pull");
    let child_id = RemoteId::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let child_path = fixture.child_file("roadmap");
    assert!(child_path.exists());

    let report = run_pull(
        &mut store,
        &fixture.connector_missing_page(child_id.as_str()),
        &child_path,
    )
    .expect("pull deleted child");

    assert!(report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 0);
    assert!(!child_path.exists());
    assert!(
        store
            .get_entity(&fixture.mount_id, &child_id)
            .expect("get deleted entity")
            .is_none()
    );
}

#[test]
fn pull_file_writes_inline_conflict_markers_and_marks_conflicted_when_remote_changed() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    run_pull(&mut store, &fixture.connector("Roadmap"), &fixture.root).expect("initial pull");
    fs::write(
        fixture.root_file("roadmap"),
        "---\nloc:\n  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\nLocal edit.\n",
    )
    .expect("dirty write");

    let report = run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body.", "2026-06-11T00:00:00.000Z"),
        fixture.root_file("roadmap"),
    )
    .expect("pull conflicted file");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(
        report.conflicts[0].path,
        fixture.root_file("roadmap").display().to_string()
    );
    assert_eq!(
        report.conflicts[0].remote_id,
        fixture.canonical_root_page_id.as_str()
    );
    let contents = fs::read_to_string(fixture.root_file("roadmap")).expect("local file");
    assert!(contents.contains("Local edit."));
    assert!(contents.contains("Remote body."));
    assert!(contents.contains(CONFLICT_LOCAL_MARKER));
    assert!(contents.contains(CONFLICT_SEPARATOR_MARKER));
    assert!(contents.contains(CONFLICT_REMOTE_MARKER));
    assert!(has_unresolved_conflict_markers(&contents));
    assert!(
        !fixture
            .root_file("roadmap")
            .with_extension("remote.md")
            .exists()
    );
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Conflicted);
    assert_eq!(
        entity.remote_edited_at.as_deref(),
        Some("2026-06-11T00:00:00.000Z")
    );
    let shadow = store
        .load_shadow(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("load shadow");
    assert!(shadow.rendered_body.contains("Remote body."));
}

#[test]
fn pull_file_leaves_inline_conflict_unchanged_when_remote_changes_again() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    fixture.mount(&mut store);
    run_pull(&mut store, &fixture.connector("Roadmap"), &fixture.root).expect("initial pull");
    fs::write(
        fixture.root_file("roadmap"),
        "---\nloc:\n  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n  type: page\n  synced_at: now\n  remote_edited_at: now\ntitle: Roadmap\n---\nLocal edit.\n",
    )
    .expect("dirty write");

    let conflicted_connector =
        fixture.connector_with("Roadmap", "Remote body.", "2026-06-11T00:00:00.000Z");
    run_pull(
        &mut store,
        &conflicted_connector,
        fixture.root_file("roadmap"),
    )
    .expect("pull conflicted file");
    let conflicted_contents =
        fs::read_to_string(fixture.root_file("roadmap")).expect("conflict file");

    let report = run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body v2.", "2026-06-12T00:00:00.000Z"),
        fixture.root_file("roadmap"),
    )
    .expect("pull unresolved conflict");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(
        report.conflicts[0].path,
        fixture.root_file("roadmap").display().to_string()
    );
    assert_eq!(
        fs::read_to_string(fixture.root_file("roadmap")).expect("conflict file"),
        conflicted_contents
    );
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Conflicted);
}

#[test]
fn pull_file_refreshes_inline_conflict_when_same_remote_version_shadow_drifted() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    let version = "2026-06-11T00:00:00.000Z";
    fixture.mount(&mut store);
    run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Old body.", version),
        &fixture.root,
    )
    .expect("initial pull");
    fs::write(
        fixture.root_file("roadmap"),
        concat!(
            "---\n",
            "loc:\n",
            "  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n",
            "  type: page\n",
            "  synced_at: \"2026-06-11T00:00:00.000Z\"\n",
            "  remote_edited_at: \"2026-06-11T00:00:00.000Z\"\n",
            "title: Roadmap\n",
            "---\n",
            "<<<<<<< LOCAL\n",
            "# Roadmap\n\n",
            "Local edit.\n",
            "=======\n",
            "# Roadmap\n\n",
            "Old body.\n",
            ">>>>>>> REMOTE\n",
        ),
    )
    .expect("stale conflict file");
    let mut entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get entity")
        .expect("entity");
    entity.hydration = HydrationState::Conflicted;
    entity.remote_edited_at = Some(version.to_string());
    store.save_entity(entity).expect("save conflicted entity");

    let report = run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body.\n\n---", version),
        fixture.root_file("roadmap"),
    )
    .expect("pull unresolved stale conflict");

    assert!(!report.ok);
    assert_eq!(report.hydrated, 0);
    assert_eq!(report.skipped_dirty, 1);
    assert_eq!(report.conflicts.len(), 1);
    let contents = fs::read_to_string(fixture.root_file("roadmap")).expect("conflict file");
    assert!(contents.contains("Local edit."), "{contents}");
    assert!(contents.contains("Remote body.<br><br>---"), "{contents}");
    assert!(has_unresolved_conflict_markers(&contents), "{contents}");
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Conflicted);
    assert_eq!(entity.remote_edited_at.as_deref(), Some(version));
    let shadow = store
        .load_shadow(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("load shadow");
    assert!(shadow.rendered_body.contains("Remote body.<br><br>---"));
}

#[test]
fn pull_file_compacts_same_version_inline_conflict_when_shadow_is_current() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    let version = "2026-06-11T00:00:00.000Z";
    fixture.mount(&mut store);
    run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Shared before.\n\n---\n\nShared after.", version),
        &fixture.root,
    )
    .expect("initial pull");
    let current = fs::read_to_string(fixture.root_file("roadmap")).expect("initial file");
    let body_start = current
        .match_indices("---\n")
        .nth(1)
        .expect("frontmatter end")
        .0
        + "---\n".len();
    let frontmatter = &current[..body_start];
    let remote_body = &current[body_start..];
    let local_body = remote_body.replace(
        "Shared before.<br><br>---<br><br>Shared after.",
        "Shared before.<br><br>Shared after.",
    );
    fs::write(
        fixture.root_file("roadmap"),
        format!(
            "{frontmatter}{CONFLICT_LOCAL_MARKER}\n{local_body}{CONFLICT_SEPARATOR_MARKER}\n{remote_body}{CONFLICT_REMOTE_MARKER}\n"
        ),
    )
    .expect("wide conflict file");
    let mut entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get entity")
        .expect("entity");
    entity.hydration = HydrationState::Conflicted;
    entity.remote_edited_at = Some(version.to_string());
    store.save_entity(entity).expect("save conflicted entity");

    let report = run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Shared before.\n\n---\n\nShared after.", version),
        fixture.root_file("roadmap"),
    )
    .expect("pull same-version conflict");

    assert!(!report.ok);
    assert_eq!(report.conflicts.len(), 1);
    let contents = fs::read_to_string(fixture.root_file("roadmap")).expect("conflict file");
    assert_eq!(
        contents.matches(CONFLICT_LOCAL_MARKER).count(),
        1,
        "{contents}"
    );
    assert!(
        contents.contains(concat!(
            "<<<<<<< LOCAL\n",
            "Shared before.<br><br>Shared after.\n",
            "=======\n",
            "Shared before.<br><br>---<br><br>Shared after.\n",
            ">>>>>>> REMOTE\n",
            "\n[Design Notes]",
        )),
        "{contents}"
    );
    assert!(
        !contents.contains("[Design Notes](https://www.notion.so/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb)\n\n::loc{id=cccccccccccccccccccccccccccccccc type=child_database title=\"Tasks\"}\n>>>>>>> REMOTE"),
        "{contents}"
    );
}

#[test]
fn pull_file_cleans_same_version_empty_conflict_when_content_matches_remote() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    let version = "2026-06-11T00:00:00.000Z";
    fixture.mount(&mut store);
    run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Shared body.", version),
        &fixture.root,
    )
    .expect("initial pull");
    let current = fs::read_to_string(fixture.root_file("roadmap")).expect("initial file");
    fs::write(
        fixture.root_file("roadmap"),
        format!("{current}{CONFLICT_LOCAL_MARKER}\n{CONFLICT_SEPARATOR_MARKER}\n{CONFLICT_REMOTE_MARKER}\n"),
    )
    .expect("empty conflict file");
    let mut entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get entity")
        .expect("entity");
    entity.hydration = HydrationState::Conflicted;
    entity.remote_edited_at = Some(version.to_string());
    store.save_entity(entity).expect("save conflicted entity");

    let report = run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Shared body.", version),
        fixture.root_file("roadmap"),
    )
    .expect("pull same-version empty conflict");

    assert!(report.ok);
    assert_eq!(report.conflicts.len(), 0);
    assert_eq!(report.hydrated, 1);
    assert_eq!(report.skipped_dirty, 0);
    let contents = fs::read_to_string(fixture.root_file("roadmap")).expect("conflict file");
    assert_eq!(contents, current);
    assert!(!has_unresolved_conflict_markers(&contents), "{contents}");
}

#[test]
fn pull_file_refreshes_nested_inline_conflict_when_shadow_is_current() {
    let fixture = PullFixture::new();
    let mut store = InMemoryStateStore::new();
    let version = "2026-06-11T00:00:00.000Z";
    fixture.mount(&mut store);
    run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body.\n\n---", version),
        &fixture.root,
    )
    .expect("initial pull");
    fs::write(
        fixture.root_file("roadmap"),
        concat!(
            "---\n",
            "loc:\n",
            "  id: aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n",
            "  type: page\n",
            "  synced_at: \"2026-06-11T00:00:00.000Z\"\n",
            "  remote_edited_at: \"2026-06-11T00:00:00.000Z\"\n",
            "title: Roadmap\n",
            "---\n",
            "<<<<<<< LOCAL\n",
            "<<<<<<< LOCAL\n",
            "# Roadmap\n\n",
            "Local edit.\n",
            "=======\n",
            "# Roadmap\n\n",
            "Old body.\n",
            ">>>>>>> REMOTE\n",
            "=======\n",
            "# Roadmap\n\n",
            "Old body.\n",
            ">>>>>>> REMOTE\n",
        ),
    )
    .expect("nested conflict file");
    let mut entity = store
        .get_entity(&fixture.mount_id, &fixture.canonical_root_page_id)
        .expect("get entity")
        .expect("entity");
    entity.hydration = HydrationState::Conflicted;
    entity.remote_edited_at = Some(version.to_string());
    store.save_entity(entity).expect("save conflicted entity");

    let report = run_pull(
        &mut store,
        &fixture.connector_with("Roadmap", "Remote body.\n\n---", version),
        fixture.root_file("roadmap"),
    )
    .expect("pull nested stale conflict");

    assert!(!report.ok);
    assert_eq!(report.conflicts.len(), 1);
    let contents = fs::read_to_string(fixture.root_file("roadmap")).expect("conflict file");
    assert_eq!(
        contents.matches(CONFLICT_LOCAL_MARKER).count(),
        1,
        "{contents}"
    );
    assert!(contents.contains("Local edit."), "{contents}");
    assert!(contents.contains("Remote body.<br><br>---"), "{contents}");
    assert!(has_unresolved_conflict_markers(&contents), "{contents}");
}

struct PullFixture {
    root: PathBuf,
    mount_id: MountId,
    root_page_id: RemoteId,
    canonical_root_page_id: RemoteId,
}

impl PullFixture {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "loc-cli-pull-{}-{unique}-{suffix}",
            std::process::id()
        ));

        Self {
            root,
            mount_id: MountId::new("notion-main"),
            root_page_id: RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            canonical_root_page_id: RemoteId::new("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
        }
    }

    fn mount(&self, store: &mut InMemoryStateStore) {
        self.mount_with_projection(store, ProjectionMode::PlainFiles);
    }

    fn mount_with_projection(&self, store: &mut InMemoryStateStore, projection: ProjectionMode) {
        run_mount(
            store,
            MountOptions {
                mount_id: self.mount_id.clone(),
                connector: "notion".to_string(),
                root: self.root.clone(),
                remote_root_id: Some(self.root_page_id.clone()),
                connection_id: None,
                read_only: false,
                projection,
            },
        )
        .expect("mount");
        assert_eq!(store.load_mounts().expect("mounts").len(), 1);
    }

    fn mount_config(&self, projection: ProjectionMode) -> MountConfig {
        MountConfig::new(self.mount_id.clone(), "notion", self.root.clone())
            .with_remote_root_id(self.root_page_id.clone())
            .projection(projection)
    }

    fn connector(&self, root_title: &str) -> NotionConnector {
        self.connector_with(root_title, "Root body.", "2026-06-10T00:00:00.000Z")
    }

    fn connector_with(
        &self,
        root_title: &str,
        root_body: &str,
        last_edited_time: &str,
    ) -> NotionConnector {
        NotionConnector::with_api(
            NotionConfig::default(),
            Arc::new(FixtureNotionApi::new(
                self.root_page_id.as_str(),
                self.canonical_root_page_id.as_str(),
                root_title,
                root_body,
                last_edited_time,
            )),
        )
    }

    fn connector_with_duplicate_child(&self, root_title: &str) -> NotionConnector {
        NotionConnector::with_api(
            NotionConfig::default(),
            Arc::new(FixtureNotionApi::new_with_duplicate_child(
                self.root_page_id.as_str(),
                self.canonical_root_page_id.as_str(),
                root_title,
                "Root body.",
                "2026-06-11T00:00:00.000Z",
            )),
        )
    }

    fn connector_with_duplicate_row(&self, root_title: &str) -> NotionConnector {
        NotionConnector::with_api(
            NotionConfig::default(),
            Arc::new(FixtureNotionApi::new_with_duplicate_row(
                self.root_page_id.as_str(),
                self.canonical_root_page_id.as_str(),
                root_title,
                "Root body.",
                "2026-06-11T00:00:00.000Z",
            )),
        )
    }

    fn connector_with_row_count(&self, root_title: &str, row_count: usize) -> NotionConnector {
        NotionConnector::with_api(
            NotionConfig::default(),
            Arc::new(
                FixtureNotionApi::new(
                    self.root_page_id.as_str(),
                    self.canonical_root_page_id.as_str(),
                    root_title,
                    "Root body.",
                    "2026-06-11T00:00:00.000Z",
                )
                .with_database_row_count(row_count),
            ),
        )
    }

    fn connector_with_nested_child(&self, root_title: &str) -> NotionConnector {
        NotionConnector::with_api(
            NotionConfig::default(),
            Arc::new(
                FixtureNotionApi::new(
                    self.root_page_id.as_str(),
                    self.canonical_root_page_id.as_str(),
                    root_title,
                    "Root body.",
                    "2026-06-11T00:00:00.000Z",
                )
                .with_nested_child(),
            ),
        )
    }

    fn connector_missing_page(&self, missing_page_id: &str) -> NotionConnector {
        let mut api = FixtureNotionApi::new(
            self.root_page_id.as_str(),
            self.canonical_root_page_id.as_str(),
            "Roadmap",
            "Root body.",
            "2026-06-11T00:00:00.000Z",
        );
        api.pages.remove(missing_page_id);
        NotionConnector::with_api(NotionConfig::default(), Arc::new(api))
    }

    fn root_file(&self, slug: &str) -> PathBuf {
        self.root.join(slug).join("page.md")
    }

    fn child_file(&self, root_slug: &str) -> PathBuf {
        self.root
            .join(root_slug)
            .join("design-notes")
            .join("page.md")
    }

    fn mount_point_root(&self) -> PathBuf {
        virtual_projection_mount_point(&self.mount_config(ProjectionMode::LinuxFuse))
    }

    fn mount_point_child_file(&self, root_slug: &str) -> PathBuf {
        self.mount_point_root()
            .join(root_slug)
            .join("design-notes")
            .join("page.md")
    }

    fn database_schema_file(&self) -> PathBuf {
        self.root.join("roadmap").join("tasks").join("_schema.yaml")
    }

    fn mount_point_database_dir(&self) -> PathBuf {
        self.mount_point_root().join("roadmap").join("tasks")
    }

    fn row_file(&self) -> PathBuf {
        self.root
            .join("roadmap")
            .join("tasks")
            .join("fix-login-bug")
            .join("page.md")
    }

    fn colliding_child_file(&self, root_slug: &str, short_id: &str) -> PathBuf {
        self.root
            .join(root_slug)
            .join(format!("design-notes {short_id}"))
            .join("page.md")
    }

    fn colliding_row_file(&self, short_id: &str) -> PathBuf {
        self.root
            .join("roadmap")
            .join("tasks")
            .join(format!("fix-login-bug {short_id}"))
            .join("page.md")
    }
}

impl Drop for PullFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn unique_temp_path(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{unique}-{suffix}", std::process::id()))
}

#[cfg(target_os = "macos")]
fn seed_remote_fast_forward_cache(
    store: &mut InMemoryStateStore,
    content_path: &PathBuf,
    mount_id: &MountId,
    remote_id: &RemoteId,
    body: &str,
) {
    let markdown_body = format!("# Roadmap\n\n{body}\n");
    let shadow = locality_core::shadow::ShadowDocument::from_synced_body(
        remote_id.clone(),
        markdown_body.clone(),
        7,
        [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
    )
    .expect("shadow")
    .with_frontmatter(root_frontmatter(remote_id, "2026-06-11T00:00:00.000Z"));
    let rendered = render_canonical_markdown(&CanonicalDocument::new(
        root_frontmatter(remote_id, "2026-06-11T00:00:00.000Z"),
        markdown_body,
    ));
    fs::write(content_path, rendered).expect("write daemon cache");
    store
        .save_shadow(mount_id, shadow.clone())
        .expect("save shadow");
    let mut entity = store
        .get_entity(mount_id, remote_id)
        .expect("get entity")
        .expect("entity");
    entity.content_hash = Some(shadow.body_hash);
    entity.remote_edited_at = Some("2026-06-11T00:00:00.000Z".to_string());
    store.save_entity(entity).expect("save entity");
}

fn root_frontmatter(remote_id: &RemoteId, remote_edited_at: &str) -> String {
    format!(
        "loc:\n  id: {}\n  type: page\n  synced_at: {}\n  remote_edited_at: {}\ntitle: Roadmap\n",
        remote_id.as_str(),
        remote_edited_at,
        remote_edited_at
    )
}

#[derive(Debug)]
struct FixtureNotionApi {
    pages: BTreeMap<String, PageDto>,
    children: BTreeMap<(String, Option<String>), BlockListDto>,
    databases: BTreeMap<String, DatabaseDto>,
    data_sources: BTreeMap<String, DataSourceDto>,
    data_source_pages: BTreeMap<(String, Option<String>), PageListDto>,
}

impl FixtureNotionApi {
    fn new(
        requested_root_page_id: &str,
        returned_root_page_id: &str,
        root_title: &str,
        root_body: &str,
        last_edited_time: &str,
    ) -> Self {
        let child_page_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let database_id = "cccccccccccccccccccccccccccccccc";
        let data_source_id = "dddddddddddddddddddddddddddddddd";
        let row_page_id = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        let pages = BTreeMap::from([
            (
                requested_root_page_id.to_string(),
                page(returned_root_page_id, root_title, last_edited_time),
            ),
            (
                returned_root_page_id.to_string(),
                page(returned_root_page_id, root_title, last_edited_time),
            ),
            (
                child_page_id.to_string(),
                page(child_page_id, "Design Notes", last_edited_time),
            ),
            (
                row_page_id.to_string(),
                database_row_page(row_page_id, "Fix login bug", last_edited_time),
            ),
        ]);
        let children = BTreeMap::from([
            (
                (returned_root_page_id.to_string(), None),
                PaginatedListDto {
                    results: vec![
                        paragraph_block("paragraph-1", root_body),
                        child_page_block(child_page_id, "Design Notes"),
                        child_database_block(database_id, "Tasks"),
                    ],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            (
                (child_page_id.to_string(), None),
                PaginatedListDto {
                    results: vec![paragraph_block("paragraph-2", "Child body.")],
                    next_cursor: None,
                    has_more: false,
                },
            ),
            ((row_page_id.to_string(), None), PaginatedListDto::default()),
        ]);
        let databases = BTreeMap::from([(
            database_id.to_string(),
            DatabaseDto {
                id: database_id.to_string(),
                title: vec![rich_text("Tasks")],
                data_sources: vec![DataSourceSummaryDto {
                    id: data_source_id.to_string(),
                    name: Some("Tasks".to_string()),
                }],
                last_edited_time: Some(last_edited_time.to_string()),
                ..Default::default()
            },
        )]);
        let data_sources = BTreeMap::from([(
            data_source_id.to_string(),
            DataSourceDto {
                id: data_source_id.to_string(),
                name: Some("Tasks".to_string()),
                properties: BTreeMap::from([(
                    "Status".to_string(),
                    DataSourcePropertyDto {
                        id: "status-id".to_string(),
                        kind: "select".to_string(),
                        select: Some(SelectPropertySchemaDto {
                            options: vec![SelectOptionDto {
                                id: "todo-id".to_string(),
                                name: "Todo".to_string(),
                                color: None,
                            }],
                        }),
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        )]);
        let data_source_pages = BTreeMap::from([(
            (data_source_id.to_string(), None),
            PaginatedListDto {
                results: vec![database_row_page(
                    row_page_id,
                    "Fix login bug",
                    last_edited_time,
                )],
                next_cursor: None,
                has_more: false,
            },
        )]);

        Self {
            pages,
            children,
            databases,
            data_sources,
            data_source_pages,
        }
    }

    fn new_with_duplicate_child(
        requested_root_page_id: &str,
        returned_root_page_id: &str,
        root_title: &str,
        root_body: &str,
        last_edited_time: &str,
    ) -> Self {
        let mut fixture = Self::new(
            requested_root_page_id,
            returned_root_page_id,
            root_title,
            root_body,
            last_edited_time,
        );
        let duplicate_child_id = "ffffffffffffffffffffffffffffffff";
        fixture.pages.insert(
            duplicate_child_id.to_string(),
            page(duplicate_child_id, "Design Notes", last_edited_time),
        );
        fixture.children.insert(
            (duplicate_child_id.to_string(), None),
            PaginatedListDto {
                results: vec![paragraph_block("paragraph-duplicate", "Duplicate body.")],
                next_cursor: None,
                has_more: false,
            },
        );
        fixture
            .children
            .get_mut(&(returned_root_page_id.to_string(), None))
            .expect("root children")
            .results
            .push(child_page_block(duplicate_child_id, "Design Notes"));
        fixture
    }

    fn new_with_duplicate_row(
        requested_root_page_id: &str,
        returned_root_page_id: &str,
        root_title: &str,
        root_body: &str,
        last_edited_time: &str,
    ) -> Self {
        let mut fixture = Self::new(
            requested_root_page_id,
            returned_root_page_id,
            root_title,
            root_body,
            last_edited_time,
        );
        let duplicate_row_id = "ffffffffffffffffffffffffffffffff";
        fixture.pages.insert(
            duplicate_row_id.to_string(),
            database_row_page(duplicate_row_id, "Fix login bug", last_edited_time),
        );
        fixture.children.insert(
            (duplicate_row_id.to_string(), None),
            PaginatedListDto::default(),
        );
        fixture
            .data_source_pages
            .get_mut(&("dddddddddddddddddddddddddddddddd".to_string(), None))
            .expect("data source page")
            .results
            .push(database_row_page(
                duplicate_row_id,
                "Fix login bug",
                last_edited_time,
            ));
        fixture
    }

    fn with_database_row_count(mut self, row_count: usize) -> Self {
        let data_source_id = "dddddddddddddddddddddddddddddddd";
        let last_edited_time = "2026-06-11T00:00:00.000Z";
        let rows = (0..row_count)
            .map(|index| {
                let row_id = format!("eeeeeeeeeeeeeeeeeeeeeeeeeeeeee{index:02x}");
                let title = if index == 0 {
                    "Fix login bug".to_string()
                } else {
                    format!("Fix login bug {index}")
                };
                self.pages.insert(
                    row_id.clone(),
                    database_row_page(&row_id, &title, last_edited_time),
                );
                self.children
                    .insert((row_id.clone(), None), PaginatedListDto::default());
                database_row_page(&row_id, &title, last_edited_time)
            })
            .collect();
        self.data_source_pages.insert(
            (data_source_id.to_string(), None),
            PaginatedListDto {
                results: rows,
                next_cursor: None,
                has_more: false,
            },
        );
        self
    }

    fn with_nested_child(mut self) -> Self {
        let child_page_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let nested_child_id = "99999999999999999999999999999999";
        let last_edited_time = "2026-06-11T00:00:00.000Z";
        self.pages.insert(
            nested_child_id.to_string(),
            page(nested_child_id, "Appendix", last_edited_time),
        );
        self.children.insert(
            (nested_child_id.to_string(), None),
            PaginatedListDto {
                results: vec![paragraph_block("paragraph-nested", "Nested child body.")],
                next_cursor: None,
                has_more: false,
            },
        );
        self.children.insert(
            (child_page_id.to_string(), None),
            PaginatedListDto {
                results: vec![
                    paragraph_block("paragraph-2", "Child body."),
                    child_page_block(nested_child_id, "Appendix"),
                ],
                next_cursor: None,
                has_more: false,
            },
        );
        self
    }
}

impl NotionApi for FixtureNotionApi {
    fn retrieve_page(&self, page_id: &str) -> locality_core::LocalityResult<PageDto> {
        self.pages
            .get(page_id)
            .cloned()
            .ok_or_else(|| {
                locality_core::LocalityError::Io(format!(
                    "notion api returned HTTP 404 Not Found: {{\"object\":\"error\",\"status\":404,\"code\":\"object_not_found\",\"message\":\"Could not find block with ID: {page_id}.\"}}"
                ))
            })
    }

    fn retrieve_database(&self, database_id: &str) -> locality_core::LocalityResult<DatabaseDto> {
        self.databases.get(database_id).cloned().ok_or_else(|| {
            locality_core::LocalityError::InvalidState(format!("missing database {database_id}"))
        })
    }

    fn retrieve_data_source(
        &self,
        data_source_id: &str,
    ) -> locality_core::LocalityResult<DataSourceDto> {
        self.data_sources
            .get(data_source_id)
            .cloned()
            .ok_or_else(|| {
                locality_core::LocalityError::InvalidState(format!(
                    "missing data source {data_source_id}"
                ))
            })
    }

    fn query_data_source(
        &self,
        data_source_id: &str,
        start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<PageListDto> {
        Ok(self
            .data_source_pages
            .get(&(data_source_id.to_string(), start_cursor.map(str::to_string)))
            .cloned()
            .unwrap_or_default())
    }

    fn retrieve_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> locality_core::LocalityResult<BlockListDto> {
        Ok(self
            .children
            .get(&(block_id.to_string(), start_cursor.map(str::to_string)))
            .cloned()
            .unwrap_or_default())
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
        _body: serde_json::Value,
    ) -> locality_core::LocalityResult<BlockDto> {
        Err(locality_core::LocalityError::NotImplemented(
            "fixture update block",
        ))
    }

    fn append_block_children(
        &self,
        _block_id: &str,
        _body: serde_json::Value,
    ) -> locality_core::LocalityResult<BlockListDto> {
        Err(locality_core::LocalityError::NotImplemented(
            "fixture append block children",
        ))
    }

    fn delete_block(&self, _block_id: &str) -> locality_core::LocalityResult<BlockDto> {
        Err(locality_core::LocalityError::NotImplemented(
            "fixture delete block",
        ))
    }
}

fn page(id: &str, title: &str, last_edited_time: &str) -> PageDto {
    PageDto {
        id: id.to_string(),
        parent: None,
        created_time: Some("2026-06-10T00:00:00.000Z".to_string()),
        last_edited_time: Some(last_edited_time.to_string()),
        created_by: None,
        archived: false,
        in_trash: false,
        properties: BTreeMap::from([(
            "title".to_string(),
            PagePropertyDto {
                kind: "title".to_string(),
                title: vec![rich_text(title)],
                rich_text: Vec::new(),
                ..Default::default()
            },
        )]),
    }
}

fn database_row_page(id: &str, title: &str, last_edited_time: &str) -> PageDto {
    let mut page = page(id, title, last_edited_time);
    page.properties.insert(
        "Status".to_string(),
        PagePropertyDto {
            kind: "select".to_string(),
            select: Some(SelectOptionDto {
                id: "todo-id".to_string(),
                name: "Todo".to_string(),
                color: None,
            }),
            ..Default::default()
        },
    );
    page
}

fn paragraph_block(id: &str, text: &str) -> BlockDto {
    let mut block = block(id, "paragraph");
    block.paragraph = Some(RichTextBlockDto {
        rich_text: vec![rich_text(text)],
        color: None,
    });
    block
}

fn child_page_block(id: &str, title: &str) -> BlockDto {
    let mut block = block(id, "child_page");
    block.child_page = Some(TitleBlockDto {
        title: title.to_string(),
    });
    block
}

fn child_database_block(id: &str, title: &str) -> BlockDto {
    let mut block = block(id, "child_database");
    block.child_database = Some(TitleBlockDto {
        title: title.to_string(),
    });
    block
}

fn block(id: &str, kind: &str) -> BlockDto {
    BlockDto {
        id: id.to_string(),
        kind: kind.to_string(),
        ..Default::default()
    }
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
