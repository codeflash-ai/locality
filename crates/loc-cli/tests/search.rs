use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loc_cli::search::{SearchError, SearchOptions, notion_id_from_url, run_search};
use locality_core::freshness::RemoteVersion;
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, EntityRecord, EntityRepository,
    InMemoryStateStore, MountConfig, MountRepository, ProjectionMode, RemoteObservationRecord,
    RemoteObservationRepository, SqliteStateStore,
};

#[test]
fn search_ranks_title_path_and_remote_id_matches() {
    let fixture = SearchFixture::new();
    let mut store = fixture.store();
    fixture.seed_entities(&mut store);

    let title = run_search(
        &store,
        SearchOptions {
            query: "roadmap".to_string(),
            connector: Some("notion".to_string()),
            limit: 10,
            include_stale_access: false,
        },
    )
    .expect("title search");
    assert_eq!(title.results[0].title, "Roadmap 2026");

    let path = run_search(
        &store,
        SearchOptions {
            query: "product".to_string(),
            connector: Some("notion".to_string()),
            limit: 10,
            include_stale_access: false,
        },
    )
    .expect("path search");
    assert_eq!(path.results[0].title, "Initial Idea");

    let id = run_search(
        &store,
        SearchOptions {
            query:
                "https://app.notion.com/p/codeflash/Initial-Idea-37b3ac0ebb88802cbcf4d53c9cfc4972"
                    .to_string(),
            connector: Some("notion".to_string()),
            limit: 10,
            include_stale_access: false,
        },
    )
    .expect("id search");
    assert_eq!(id.results.len(), 1);
    assert_eq!(id.results[0].state, "ready");
    assert!(id.results[0].safety.agent_readable);
    assert_eq!(id.results[0].safety.labels, vec!["ready"]);
    assert!(
        Path::new(&id.results[0].absolute_path).ends_with(
            PathBuf::from("Product")
                .join("Initial Idea")
                .join("page.md")
        )
    );
}

#[test]
fn notion_id_parser_rejects_non_notion_urls_with_hex_suffixes() {
    assert_eq!(
        notion_id_from_url(
            "https://github.com/codeflash-ai/locality/commit/15e6dedcfd04d1cdb22df006b66a90dd4ab3753c",
        ),
        None
    );
    assert_eq!(
        notion_id_from_url(
            "https://app.notion.com/p/codeflash/Initial-Idea-37b3ac0ebb88802cbcf4d53c9cfc4972",
        )
        .as_deref(),
        Some("37b3ac0ebb88802cbcf4d53c9cfc4972")
    );
    assert_eq!(
        notion_id_from_url(
            "https://app.notion.com/p/codeflash/4614fba49bdf45e0a0064f91dca082f1?v=9cc7553907744e2f8fb7bfcccf9d0ddd",
        )
        .as_deref(),
        Some("4614fba49bdf45e0a0064f91dca082f1")
    );
    assert_eq!(
        notion_id_from_url("37b3ac0e-bb88-802c-bcf4-d53c9cfc4972").as_deref(),
        Some("37b3ac0ebb88802cbcf4d53c9cfc4972")
    );
    assert_eq!(
        notion_id_from_url("Locality live CLI binary 1042614-1782910284842818531"),
        None
    );
}

#[test]
fn search_does_not_treat_github_commit_links_as_notion_ids() {
    let fixture = SearchFixture::new();
    let mut store = fixture.store();
    fixture.seed_entities(&mut store);

    let report = run_search(
        &store,
        SearchOptions::new(
            "https://github.com/codeflash-ai/locality/commit/15e6dedcfd04d1cdb22df006b66a90dd4ab3753c",
        ),
    )
    .expect("github URL search");

    assert!(report.results.is_empty());
}

#[test]
fn search_uses_remote_observation_metadata_without_touching_remote() {
    let fixture = SearchFixture::new();
    let mut store = fixture.store();
    fixture.seed_entities(&mut store);
    store
        .save_remote_observation(
            RemoteObservationRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                EntityKind::Page,
                "Launch Plan",
                "Engineering/Launch Plan/page.md",
                "2026-06-16T00:00:00Z",
            )
            .with_remote_version(RemoteVersion("remote-v2".to_string())),
        )
        .expect("save observation");

    let report = run_search(&store, SearchOptions::new("launch")).expect("search");

    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].title, "Roadmap 2026");
    assert_eq!(report.results[0].state, "remote_update_available");
    assert!(!report.results[0].safety.agent_readable);
    assert_eq!(
        report.results[0].safety.labels,
        vec!["remote_changed", "stale_local"]
    );
    assert_eq!(
        report.results[0].remote.observed_title.as_deref(),
        Some("Launch Plan")
    );
}

#[test]
fn search_does_not_treat_equal_versionless_observation_as_changed() {
    let fixture = SearchFixture::new();
    let mut store = fixture.store();
    fixture.seed_entities(&mut store);
    store
        .save_remote_observation(RemoteObservationRecord::new(
            fixture.mount_id.clone(),
            RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            EntityKind::Page,
            "Roadmap 2026",
            "Engineering/Roadmap 2026/page.md",
            "2026-06-16T00:00:00Z",
        ))
        .expect("save observation");

    let report = run_search(&store, SearchOptions::new("roadmap")).expect("search");

    assert_eq!(report.results[0].title, "Roadmap 2026");
    assert_eq!(report.results[0].state, "online_only");
    assert!(!report.results[0].safety.agent_readable);
    assert_eq!(
        report.results[0].safety.labels,
        vec!["online_only", "metadata_only"]
    );
    assert!(!report.results[0].remote.changed);
}

#[test]
fn search_uses_sqlite_candidate_index_without_changing_report() {
    let fixture = SearchFixture::new();
    let mut store = fixture.sqlite_store();
    fixture.seed_entities(&mut store);
    store
        .save_remote_observation(
            RemoteObservationRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                EntityKind::Page,
                "Launch Plan",
                "Engineering/Launch Plan/page.md",
                "2026-06-16T00:00:00Z",
            )
            .with_remote_version(RemoteVersion("remote-v2".to_string())),
        )
        .expect("save observation");

    let report = run_search(&store, SearchOptions::new("launch")).expect("search");

    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].title, "Roadmap 2026");
    assert_eq!(report.results[0].state, "remote_update_available");
    assert_eq!(
        report.results[0].remote.observed_title.as_deref(),
        Some("Launch Plan")
    );
}

#[test]
fn search_matches_numeric_short_title_terms() {
    let fixture = SearchFixture::new();
    let mut store = fixture.sqlite_store();
    fixture.seed_entities(&mut store);
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("cccccccccccccccccccccccccccccccc"),
                EntityKind::Page,
                "1:1 Notes",
                "Meetings/1-1 Notes/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save numbered notes");

    let report = run_search(&store, SearchOptions::new("1")).expect("numeric search");

    assert_eq!(report.results[0].title, "1:1 Notes");
    assert_eq!(report.results[0].path, "Meetings/1-1 Notes/page.md");
}

#[test]
fn search_matches_free_text_titles_with_long_numeric_suffixes() {
    let fixture = SearchFixture::new();
    let mut store = fixture.sqlite_store();
    fixture.seed_entities(&mut store);
    let title = "Locality live CLI binary 1042614-1782910284842818531";
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                RemoteId::new("3903ac0ebb8881ea8916e16bfbcb6d7d"),
                EntityKind::Page,
                title,
                "locality-live-cli-binary-1042614-1782910284842818531/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save long numeric suffix entity");

    let report = run_search(&store, SearchOptions::new(title)).expect("title search");

    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].title, title);
    assert_eq!(
        report.results[0].path,
        "locality-live-cli-binary-1042614-1782910284842818531/page.md"
    );
}

#[test]
fn search_filters_connectors_and_rejects_empty_queries() {
    let fixture = SearchFixture::new();
    let mut store = fixture.store();
    fixture.seed_entities(&mut store);
    store
        .save_mount(MountConfig::new(
            MountId::new("linear-main"),
            "linear",
            fixture.root.join("linear"),
        ))
        .expect("save linear mount");
    store
        .save_entity(EntityRecord::new(
            MountId::new("linear-main"),
            RemoteId::new("lin-1"),
            EntityKind::Page,
            "Roadmap Linear",
            "Roadmap Linear.md",
        ))
        .expect("save linear entity");

    let notion = run_search(
        &store,
        SearchOptions {
            query: "roadmap".to_string(),
            connector: Some("notion".to_string()),
            limit: 10,
            include_stale_access: false,
        },
    )
    .expect("notion search");
    assert_eq!(notion.results.len(), 1);
    assert_eq!(notion.results[0].connector, "notion");

    let empty = run_search(&store, SearchOptions::new("   ")).expect_err("empty query");
    assert!(matches!(empty, SearchError::EmptyQuery));
}

#[test]
fn search_hides_inactive_connection_mounts_by_default() {
    let fixture = SearchFixture::new();
    let mut store = InMemoryStateStore::new();
    let current_mount_id = MountId::new("notion-current");
    let stale_mount_id = MountId::new("notion-stale");
    store
        .save_connection(test_connection("current", "active"))
        .expect("save active connection");
    store
        .save_connection(test_connection("stale", "revoked"))
        .expect("save stale connection");
    store
        .save_mount(
            MountConfig::new(
                current_mount_id.clone(),
                "notion",
                fixture.root.join("current"),
            )
            .with_connection_id(ConnectionId::new("current")),
        )
        .expect("save current mount");
    store
        .save_mount(
            MountConfig::new(stale_mount_id.clone(), "notion", fixture.root.join("stale"))
                .with_connection_id(ConnectionId::new("stale")),
        )
        .expect("save stale mount");
    store
        .save_entity(
            EntityRecord::new(
                current_mount_id.clone(),
                RemoteId::new("current-roadmap"),
                EntityKind::Page,
                "Roadmap Current",
                "Roadmap Current/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save current entity");
    store
        .save_entity(
            EntityRecord::new(
                stale_mount_id.clone(),
                RemoteId::new("stale-roadmap"),
                EntityKind::Page,
                "Roadmap Stale",
                "Roadmap Stale/page.md",
            )
            .with_hydration(HydrationState::Hydrated),
        )
        .expect("save stale entity");

    let current = run_search(&store, SearchOptions::new("roadmap")).expect("current search");
    assert_eq!(current.results.len(), 1);
    assert_eq!(current.results[0].mount_id, "notion-current");

    let all = run_search(
        &store,
        SearchOptions {
            query: "roadmap".to_string(),
            connector: None,
            limit: 10,
            include_stale_access: true,
        },
    )
    .expect("all search");
    assert_eq!(all.results.len(), 2);
    assert!(
        all.results
            .iter()
            .any(|result| result.mount_id == "notion-stale")
    );
}

#[test]
fn notion_url_locate_prefers_page_file_over_workspace_fallback() {
    let fixture = SearchFixture::new();
    let mut store = fixture.store();
    store
        .save_mount(
            MountConfig::new(
                fixture.mount_id.clone(),
                "notion",
                fixture.root.join("notion"),
            )
            .with_remote_root_id(RemoteId::new("37b3ac0ebb88802cbcf4d53c9cfc4972")),
        )
        .expect("point mount root at indexed page");
    fixture.seed_entities(&mut store);

    let report = run_search(
        &store,
        SearchOptions::new(
            "https://app.notion.com/p/codeflash/Initial-Idea-37b3ac0ebb88802cbcf4d53c9cfc4972",
        ),
    )
    .expect("locate root page URL");

    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].kind, "page");
    assert_eq!(report.results[0].path, "Product/Initial Idea/page.md");
    assert!(Path::new(&report.results[0].absolute_path).ends_with("page.md"));
}

#[test]
fn notion_url_locate_keeps_workspace_fallback_when_root_entity_is_unknown() {
    let fixture = SearchFixture::new();
    let mut store = fixture.store();
    fixture.seed_entities(&mut store);

    let report = run_search(
        &store,
        SearchOptions::new(
            "https://app.notion.com/p/codeflash/Root-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ),
    )
    .expect("locate unknown root URL");

    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].kind, "workspace");
    assert_eq!(report.results[0].path, ".");
    assert_eq!(report.results[0].state, "ready");
}

#[test]
fn search_reports_linux_fuse_absolute_path_under_mount_point_root() {
    let fixture = SearchFixture::new();
    let mut store = InMemoryStateStore::new();
    let mount = MountConfig::new(fixture.mount_id.clone(), "notion", fixture.root.clone())
        .projection(ProjectionMode::LinuxFuse);
    store
        .save_mount(mount.clone())
        .expect("save linux fuse mount");
    fixture.seed_entities(&mut store);

    let report = run_search(&store, SearchOptions::new("initial")).expect("search");

    let expected = mount
        .root
        .join("Product")
        .join("Initial Idea")
        .join("page.md")
        .display()
        .to_string();
    assert_eq!(report.results[0].absolute_path, expected);
}

#[test]
fn search_absolute_path_uses_mount_point_root() {
    let mut store = InMemoryStateStore::new();
    let mount_id = MountId::new("notion-main");
    store
        .save_mount(
            MountConfig::new(mount_id.clone(), "notion", "/tmp/Locality/notion-main")
                .projection(ProjectionMode::LinuxFuse),
        )
        .expect("save mount");
    store
        .save_entity(EntityRecord {
            mount_id: mount_id.clone(),
            remote_id: RemoteId::new("page-1"),
            kind: EntityKind::Page,
            title: "Roadmap".to_string(),
            path: PathBuf::from("Roadmap/page.md"),
            hydration: HydrationState::Hydrated,
            content_hash: None,
            remote_edited_at: None,
        })
        .expect("save entity");

    let report = run_search(
        &store,
        SearchOptions {
            query: "Roadmap".to_string(),
            connector: None,
            limit: 10,
            include_stale_access: false,
        },
    )
    .expect("search");

    let expected = locality_platform::join_logical_path(
        Path::new("/tmp/Locality/notion-main"),
        Path::new("Roadmap/page.md"),
    )
    .display()
    .to_string();
    assert_eq!(report.results[0].absolute_path, expected);
}

struct SearchFixture {
    root: PathBuf,
    mount_id: MountId,
}

impl SearchFixture {
    fn new() -> Self {
        let root = unique_temp_path("loc-cli-search");
        fs::create_dir_all(&root).expect("fixture root");
        Self {
            root,
            mount_id: MountId::new("notion-main"),
        }
    }

    fn store(&self) -> InMemoryStateStore {
        let mut store = InMemoryStateStore::new();
        store.save_mount(self.mount_config()).expect("save mount");
        store
    }

    fn sqlite_store(&self) -> SqliteStateStore {
        let mut store =
            SqliteStateStore::open(self.root.join("state")).expect("open sqlite search store");
        store.save_mount(self.mount_config()).expect("save mount");
        store
    }

    fn mount_config(&self) -> MountConfig {
        MountConfig::new(self.mount_id.clone(), "notion", self.root.join("notion"))
            .with_remote_root_id(RemoteId::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"))
    }

    fn seed_entities<S>(&self, store: &mut S)
    where
        S: EntityRepository,
    {
        store
            .save_entity(
                EntityRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new("37b3ac0ebb88802cbcf4d53c9cfc4972"),
                    EntityKind::Page,
                    "Initial Idea",
                    "Product/Initial Idea/page.md",
                )
                .with_hydration(HydrationState::Hydrated)
                .with_synced_tree_remote_version("remote-v1"),
            )
            .expect("save initial idea");
        store
            .save_entity(
                EntityRecord::new(
                    self.mount_id.clone(),
                    RemoteId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                    EntityKind::Page,
                    "Roadmap 2026",
                    "Engineering/Roadmap 2026/page.md",
                )
                .with_synced_tree_remote_version("remote-v1"),
            )
            .expect("save roadmap");
    }
}

impl Drop for SearchFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn test_connection(connection_id: &str, status: &str) -> ConnectionRecord {
    ConnectionRecord {
        connection_id: ConnectionId::new(connection_id),
        profile_id: None,
        connector: "notion".to_string(),
        display_name: connection_id.to_string(),
        account_label: Some("agent@example.com".to_string()),
        workspace_id: Some("workspace".to_string()),
        workspace_name: Some("Workspace".to_string()),
        auth_kind: "oauth".to_string(),
        secret_ref: format!("connection:{connection_id}"),
        scopes: Vec::new(),
        capabilities_json: "{}".to_string(),
        status: status.to_string(),
        created_at: "2026-06-30T00:00:00Z".to_string(),
        updated_at: "2026-06-30T00:00:00Z".to_string(),
        expires_at: None,
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_path(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{nanos}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}
