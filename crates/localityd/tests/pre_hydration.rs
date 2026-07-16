use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use locality_core::hydration::HydrationReason;
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
use locality_core::{LocalityError, LocalityResult};
use locality_store::{
    ConnectorStateRecord, ConnectorStateRepository, EntityRecord, EntityRepository,
    FreshnessStateRecord, FreshnessStateRepository, InMemoryStateStore, MountConfig,
    MountPreHydrationState, MountPreHydrationStatus, RemoteObservationRecord,
    RemoteObservationRepository, StoreError, StoreResult, load_mount_pre_hydration_state,
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

#[test]
fn pre_hydration_prefetches_only_absent_stub_and_virtual_pages() {
    let mount_id = MountId::new("notion-main");
    let mount = MountConfig::new(
        mount_id.clone(),
        "notion",
        temp_root("prehydrate-existing-states"),
    );
    let mut store = InMemoryStateStore::new();
    seed_existing_page(
        &mut store,
        &mount_id,
        "stub-page",
        "Stub/page.md",
        HydrationState::Stub,
    );
    seed_existing_page(
        &mut store,
        &mount_id,
        "virtual-page",
        "Virtual/page.md",
        HydrationState::Virtual,
    );
    seed_existing_page(
        &mut store,
        &mount_id,
        "hydrated-page",
        "Hydrated/page.md",
        HydrationState::Hydrated,
    );
    seed_existing_page(
        &mut store,
        &mount_id,
        "dirty-page",
        "Dirty/page.md",
        HydrationState::Dirty,
    );
    seed_existing_page(
        &mut store,
        &mount_id,
        "conflicted-page",
        "Conflicted/page.md",
        HydrationState::Conflicted,
    );
    let mut hydration = HydrationQueue::new();
    let mut source = FakePreHydrationSource::default();
    source.insert_entries(
        &mount_id,
        vec![
            page_entry(&mount_id, "absent-page", "Absent", "Absent/page.md"),
            page_entry(&mount_id, "stub-page", "Stub", "Stub/page.md"),
            page_entry(&mount_id, "virtual-page", "Virtual", "Virtual/page.md"),
            page_entry(&mount_id, "hydrated-page", "Hydrated", "Hydrated/page.md"),
            page_entry(&mount_id, "dirty-page", "Dirty", "Dirty/page.md"),
            page_entry(
                &mount_id,
                "conflicted-page",
                "Conflicted",
                "Conflicted/page.md",
            ),
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

    assert_eq!(report.enumerated, 6);
    assert_eq!(report.queued_hydrations, 3);
    assert_eq!(
        queued_remote_ids(&mut hydration),
        vec![
            RemoteId::new("absent-page"),
            RemoteId::new("stub-page"),
            RemoteId::new("virtual-page"),
        ]
    );

    let state = load_mount_pre_hydration_state(&store, "notion", &mount_id)
        .expect("load state")
        .expect("state");
    assert_eq!(state.status, MountPreHydrationStatus::Hydrating);
    assert_eq!(state.discovered_pages, 6);
    assert_eq!(state.queued_pages, 3);
}

#[test]
fn pre_hydration_records_error_when_hydrating_state_write_fails() {
    let mount_id = MountId::new("notion-main");
    let mount = MountConfig::new(
        mount_id.clone(),
        "notion",
        temp_root("prehydrate-state-write-error"),
    );
    let mut store = HydratingStateSaveFailureStore::new();
    let mut hydration = HydrationQueue::new();
    let mut source = FakePreHydrationSource::default();
    source.insert_entries(
        &mount_id,
        vec![page_entry(&mount_id, "page-1", "Home", "Home/page.md")],
    );

    let error = execute_mount_pre_hydration(
        &mut store,
        &mut hydration,
        &mount,
        &source,
        None,
        "2026-07-16T10:00:00Z",
    )
    .expect_err("hydrating state write fails");

    assert_eq!(error.to_string(), "io error: hydrating save failed");
    assert_eq!(store.error_save_attempts, 1);
    let state = load_mount_pre_hydration_state(&store.inner, "notion", &mount_id)
        .expect("load state")
        .expect("state");
    assert_eq!(state.status, MountPreHydrationStatus::Error);
    assert_eq!(
        state.last_error.as_deref(),
        Some("io error: hydrating save failed")
    );
}

#[test]
fn pre_hydration_delegates_database_schema_projection() {
    let mount_id = MountId::new("notion-main");
    let root = temp_root("prehydrate-schema");
    let mount = MountConfig::new(mount_id.clone(), "notion", root.clone());
    let mut store = InMemoryStateStore::new();
    let mut hydration = HydrationQueue::new();
    let mut source = FakePreHydrationSource::default();
    source.insert_entries(
        &mount_id,
        vec![database_entry(&mount_id, "db-1", "Tasks", "Home/Tasks")],
    );
    source.insert_schema("db-1", "title: Tasks\nproperties: {}\n");

    let report = execute_mount_pre_hydration(
        &mut store,
        &mut hydration,
        &mount,
        &source,
        None,
        "2026-07-16T10:00:00Z",
    )
    .expect("execute pre-hydration");

    assert_eq!(report.enumerated, 1);
    assert_eq!(report.schemas_written, 1);
    assert_eq!(
        std::fs::read_to_string(root.join("Home/Tasks/_schema.yaml")).expect("schema contents"),
        "title: Tasks\nproperties: {}\n"
    );
    assert!(hydration.is_empty());
}

#[derive(Default)]
struct FakePreHydrationSource {
    entries: BTreeMap<MountId, Vec<TreeEntry>>,
    schemas: BTreeMap<RemoteId, String>,
    enumerated: RefCell<Vec<MountId>>,
}

impl FakePreHydrationSource {
    fn insert_entries(&mut self, mount_id: &MountId, entries: Vec<TreeEntry>) {
        self.entries.insert(mount_id.clone(), entries);
    }

    fn enumerated_mounts(&self) -> Vec<MountId> {
        self.enumerated.borrow().clone()
    }

    fn insert_schema(&mut self, remote_id: &str, schema: &str) {
        self.schemas
            .insert(RemoteId::new(remote_id), schema.to_string());
    }
}

impl ScheduledPullSource for FakePreHydrationSource {
    fn enumerate_mount(&self, mount: &MountConfig) -> LocalityResult<Vec<TreeEntry>> {
        self.enumerated.borrow_mut().push(mount.mount_id.clone());
        self.entries.get(&mount.mount_id).cloned().ok_or_else(|| {
            LocalityError::InvalidState(format!("missing fixture for {}", mount.mount_id.0))
        })
    }

    fn database_schema_yaml(
        &self,
        _mount: &MountConfig,
        remote_id: &RemoteId,
    ) -> LocalityResult<Option<String>> {
        Ok(self.schemas.get(remote_id).cloned())
    }
}

struct HydratingStateSaveFailureStore {
    inner: InMemoryStateStore,
    error_save_attempts: usize,
}

impl HydratingStateSaveFailureStore {
    fn new() -> Self {
        Self {
            inner: InMemoryStateStore::new(),
            error_save_attempts: 0,
        }
    }
}

impl ConnectorStateRepository for HydratingStateSaveFailureStore {
    fn save_connector_state(&mut self, state: ConnectorStateRecord) -> StoreResult<()> {
        let pre_hydration = serde_json::from_str::<MountPreHydrationState>(&state.state_json)
            .expect("pre-hydration state json");
        match pre_hydration.status {
            MountPreHydrationStatus::Hydrating => {
                Err(StoreError::Database("hydrating save failed".to_string()))
            }
            MountPreHydrationStatus::Error => {
                self.error_save_attempts += 1;
                self.inner.save_connector_state(state)
            }
            MountPreHydrationStatus::Requested
            | MountPreHydrationStatus::Enumerating
            | MountPreHydrationStatus::Complete => self.inner.save_connector_state(state),
        }
    }

    fn get_connector_state(
        &self,
        connector: &str,
        scope_kind: &str,
        scope_id: &str,
    ) -> StoreResult<Option<ConnectorStateRecord>> {
        self.inner
            .get_connector_state(connector, scope_kind, scope_id)
    }
}

impl EntityRepository for HydratingStateSaveFailureStore {
    fn save_entity(&mut self, entity: EntityRecord) -> StoreResult<()> {
        self.inner.save_entity(entity)
    }

    fn get_entity(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<EntityRecord>> {
        self.inner.get_entity(mount_id, remote_id)
    }

    fn find_entity_by_path(
        &self,
        mount_id: &MountId,
        path: &std::path::Path,
    ) -> StoreResult<Option<EntityRecord>> {
        self.inner.find_entity_by_path(mount_id, path)
    }

    fn list_entities(&self, mount_id: &MountId) -> StoreResult<Vec<EntityRecord>> {
        self.inner.list_entities(mount_id)
    }

    fn delete_entity(&mut self, mount_id: &MountId, remote_id: &RemoteId) -> StoreResult<()> {
        self.inner.delete_entity(mount_id, remote_id)
    }
}

impl RemoteObservationRepository for HydratingStateSaveFailureStore {
    fn save_remote_observation(&mut self, observation: RemoteObservationRecord) -> StoreResult<()> {
        self.inner.save_remote_observation(observation)
    }

    fn get_remote_observation(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<RemoteObservationRecord>> {
        self.inner.get_remote_observation(mount_id, remote_id)
    }

    fn list_remote_observations(
        &self,
        mount_id: &MountId,
    ) -> StoreResult<Vec<RemoteObservationRecord>> {
        self.inner.list_remote_observations(mount_id)
    }

    fn delete_remote_observation(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<()> {
        self.inner.delete_remote_observation(mount_id, remote_id)
    }
}

impl FreshnessStateRepository for HydratingStateSaveFailureStore {
    fn save_freshness_state(&mut self, state: FreshnessStateRecord) -> StoreResult<()> {
        self.inner.save_freshness_state(state)
    }

    fn get_freshness_state(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<FreshnessStateRecord>> {
        self.inner.get_freshness_state(mount_id, remote_id)
    }

    fn list_freshness_states(&self, mount_id: &MountId) -> StoreResult<Vec<FreshnessStateRecord>> {
        self.inner.list_freshness_states(mount_id)
    }

    fn delete_freshness_state(
        &mut self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<()> {
        self.inner.delete_freshness_state(mount_id, remote_id)
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

fn seed_existing_page(
    store: &mut InMemoryStateStore,
    mount_id: &MountId,
    remote_id: &str,
    path: &str,
    hydration: HydrationState,
) {
    store
        .save_entity(
            EntityRecord::new(
                mount_id.clone(),
                RemoteId::new(remote_id),
                EntityKind::Page,
                remote_id,
                path,
            )
            .with_hydration(hydration),
        )
        .expect("save existing page");
}

fn queued_remote_ids(hydration: &mut HydrationQueue) -> Vec<RemoteId> {
    let mut remote_ids = Vec::new();
    while let Some(request) = hydration.pop_ready() {
        assert_eq!(request.reason, HydrationReason::Prefetch);
        remote_ids.push(request.remote_id);
    }
    remote_ids
}

fn temp_root(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("loc-{label}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp root");
    root
}
