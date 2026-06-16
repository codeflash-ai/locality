use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_cli::search::{SearchError, SearchOptions, run_search};
use afs_core::freshness::RemoteVersion;
use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use afs_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    RemoteObservationRecord, RemoteObservationRepository, SqliteStateStore,
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
        },
    )
    .expect("id search");
    assert_eq!(id.results.len(), 1);
    assert_eq!(id.results[0].state, "ready");
    assert!(
        id.results[0]
            .absolute_path
            .ends_with("Product/Initial Idea ~37b3ac.md")
    );
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
                "Engineering/Launch Plan ~aaaaaa.md",
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
            "Engineering/Roadmap 2026 ~aaaaaa.md",
            "2026-06-16T00:00:00Z",
        ))
        .expect("save observation");

    let report = run_search(&store, SearchOptions::new("roadmap")).expect("search");

    assert_eq!(report.results[0].title, "Roadmap 2026");
    assert_eq!(report.results[0].state, "online_only");
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
                "Engineering/Launch Plan ~aaaaaa.md",
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
        },
    )
    .expect("notion search");
    assert_eq!(notion.results.len(), 1);
    assert_eq!(notion.results[0].connector, "notion");

    let empty = run_search(&store, SearchOptions::new("   ")).expect_err("empty query");
    assert!(matches!(empty, SearchError::EmptyQuery));
}

struct SearchFixture {
    root: PathBuf,
    mount_id: MountId,
}

impl SearchFixture {
    fn new() -> Self {
        let root = unique_temp_path("afs-cli-search");
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
                    "Product/Initial Idea ~37b3ac.md",
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
                    "Engineering/Roadmap 2026 ~aaaaaa.md",
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
