use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use locality_core::hydration::HydrationReason;
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    EntityRepository, InMemoryStateStore, MountConfig, MountPreHydrationStatus,
    load_mount_pre_hydration_state,
};
use localityd::hydration::HydrationQueue;
use localityd::pre_hydration::execute_mount_pre_hydration;
use localityd::reconcile::ScheduledPullSource;

#[test]
fn pre_hydration_enumerates_workspace_mount_and_queues_all_pages_as_prefetch() {
    let mount_id = MountId::new("notion-main");
    let mount = MountConfig::new(
        mount_id.clone(),
        "notion",
        temp_root("prehydrate-workspace"),
    );
    let mut store = InMemoryStateStore::new();
    let mut hydration = HydrationQueue::new();
    let mut source = FakePreHydrationSource::default();
    source.insert_entries(
        &mount_id,
        vec![
            page_entry(&mount_id, "page-1", "Home", "Home/page.md"),
            page_entry(&mount_id, "page-2", "Roadmap", "Home/Roadmap/page.md"),
            database_entry(&mount_id, "db-1", "Tasks", "Home/Tasks"),
        ],
    );

    let report = execute_mount_pre_hydration(
        &mut store,
        &mut hydration,
        &mount,
        &source,
        None,
        "2026-07-16T10:00:00Z",
    )
    .expect("execute pre-hydration");

    assert_eq!(report.enumerated, 3);
    assert_eq!(report.queued_hydrations, 2);
    assert_eq!(source.enumerated_mounts(), vec![mount_id.clone()]);
    assert_eq!(
        store
            .get_entity(&mount_id, &RemoteId::new("page-2"))
            .expect("get page")
            .expect("page")
            .hydration,
        HydrationState::Stub
    );

    let first = hydration.pop_ready().expect("first queued page");
    let second = hydration.pop_ready().expect("second queued page");
    assert_eq!(first.reason, HydrationReason::Prefetch);
    assert_eq!(second.reason, HydrationReason::Prefetch);
    assert!(hydration.is_empty());

    let state = load_mount_pre_hydration_state(&store, "notion", &mount_id)
        .expect("load state")
        .expect("state");
    assert_eq!(state.status, MountPreHydrationStatus::Hydrating);
    assert_eq!(state.discovered_pages, 2);
    assert_eq!(state.queued_pages, 2);
}

#[test]
fn pre_hydration_records_error_when_enumeration_fails() {
    let mount_id = MountId::new("notion-main");
    let mount = MountConfig::new(mount_id.clone(), "notion", temp_root("prehydrate-error"));
    let mut store = InMemoryStateStore::new();
    let mut hydration = HydrationQueue::new();
    let source = FakePreHydrationSource::default();

    let error = execute_mount_pre_hydration(
        &mut store,
        &mut hydration,
        &mount,
        &source,
        None,
        "2026-07-16T10:00:00Z",
    )
    .expect_err("missing source fixture fails");

    assert!(
        error
            .to_string()
            .contains("missing fixture for notion-main")
    );
    let state = load_mount_pre_hydration_state(&store, "notion", &mount_id)
        .expect("load state")
        .expect("state");
    assert_eq!(state.status, MountPreHydrationStatus::Error);
    assert_eq!(
        state.last_error.as_deref(),
        Some("invalid state: missing fixture for notion-main")
    );
}

#[test]
fn pre_hydration_large_mount_enumerates_once_and_records_page_counters_only() {
    let mount_id = MountId::new("notion-main");
    let mount = MountConfig::new(mount_id.clone(), "notion", temp_root("prehydrate-large"));
    let mut store = InMemoryStateStore::new();
    let mut hydration = HydrationQueue::new();
    let mut source = FakePreHydrationSource::default();
    let page_count = 128_u64;
    let mut entries = Vec::new();
    for index in 0..page_count {
        entries.push(page_entry(
            &mount_id,
            &format!("page-{index}"),
            &format!("Page {index}"),
            &format!("Page {index}/page.md"),
        ));
    }
    entries.push(database_entry(&mount_id, "db-1", "Tasks", "Tasks"));
    source.insert_entries(&mount_id, entries);

    let report = execute_mount_pre_hydration(
        &mut store,
        &mut hydration,
        &mount,
        &source,
        None,
        "2026-07-16T10:00:00Z",
    )
    .expect("execute pre-hydration");

    assert_eq!(source.enumerated_mounts(), vec![mount_id.clone()]);
    assert_eq!(report.enumerated, page_count as usize + 1);
    assert_eq!(report.queued_hydrations, page_count as usize);
    assert_eq!(hydration.len(), page_count as usize);

    let state = load_mount_pre_hydration_state(&store, "notion", &mount_id)
        .expect("load state")
        .expect("state");
    assert_eq!(state.status, MountPreHydrationStatus::Hydrating);
    assert_eq!(state.discovered_pages, page_count);
    assert_eq!(state.queued_pages, page_count);
}

#[derive(Default)]
struct FakePreHydrationSource {
    entries: BTreeMap<MountId, Vec<TreeEntry>>,
    enumerated: RefCell<Vec<MountId>>,
}

impl FakePreHydrationSource {
    fn insert_entries(&mut self, mount_id: &MountId, entries: Vec<TreeEntry>) {
        self.entries.insert(mount_id.clone(), entries);
    }

    fn enumerated_mounts(&self) -> Vec<MountId> {
        self.enumerated.borrow().clone()
    }
}

impl ScheduledPullSource for FakePreHydrationSource {
    fn enumerate_mount(&self, mount: &MountConfig) -> LocalityResult<Vec<TreeEntry>> {
        self.enumerated.borrow_mut().push(mount.mount_id.clone());
        self.entries.get(&mount.mount_id).cloned().ok_or_else(|| {
            LocalityError::InvalidState(format!("missing fixture for {}", mount.mount_id.0))
        })
    }
}

fn page_entry(mount_id: &MountId, remote_id: &str, title: &str, path: &str) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(remote_id),
        kind: EntityKind::Page,
        title: title.to_string(),
        path: PathBuf::from(path),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some("2026-06-10T00:00:00Z".to_string()),
        stub_frontmatter: None,
    }
}

fn database_entry(mount_id: &MountId, remote_id: &str, title: &str, path: &str) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(remote_id),
        kind: EntityKind::Database,
        title: title.to_string(),
        path: PathBuf::from(path),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some("2026-06-10T00:00:00Z".to_string()),
        stub_frontmatter: None,
    }
}

fn temp_root(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("loc-{label}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp root");
    root
}
