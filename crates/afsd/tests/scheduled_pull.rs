use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use afs_core::hydration::HydrationReason;
use afs_core::model::{
    CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
};
use afs_core::{AfsError, AfsResult};
use afs_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    ProjectionMode,
};
use afsd::execution::{AdvanceScheduledPullJob, DaemonExecutor};
use afsd::hydration::HydrationQueue;
use afsd::reconcile::{
    DefaultFetchScheduleStrategy, EntityFetchPlan, EntityFetchSchedule, FetchScheduleStrategy,
    MountFetchPlan, MountFetchSchedule, ScheduledPullSource,
};
use afsd::scheduler::PullScheduler;
use afsd::supervisor::DaemonSupervisor;
use afsd::watcher::FileWatcher;

#[test]
fn scheduled_pull_refreshes_projection_and_queues_default_policy_hydration() {
    let root = temp_root("scheduled-pull-default");
    let mount_id = MountId::new("notion-main");
    let mount = MountConfig::new(mount_id.clone(), "notion", root.clone())
        .with_remote_root_id(RemoteId::new("root-page"));
    let mut source = FakeScheduledPullSource::default();
    source.insert_entries(
        &mount_id,
        vec![
            page_entry(
                &mount_id,
                "root-page",
                "Home",
                "Home.md",
                "2026-06-10T00:00:00Z",
            ),
            page_entry(
                &mount_id,
                "child-page",
                "Child",
                "Home/Child.md",
                "2026-06-10T00:00:00Z",
            ),
            database_entry(&mount_id, "tasks-db", "Tasks", "Home/Tasks ~tasks"),
        ],
    );
    source.insert_schema("tasks-db", "title: Tasks\nproperties: {}\n");
    let mut supervisor = supervisor_with_mounts([mount]);

    supervisor.start().expect("start supervisor");
    let report = supervisor
        .advance_and_execute_scheduled_pull(
            AdvanceScheduledPullJob::new(Duration::ZERO),
            &source,
            &DefaultFetchScheduleStrategy,
        )
        .expect("scheduled pull");

    assert_eq!(report.mounts_checked, 1);
    assert_eq!(report.mounts_polled, 1);
    assert_eq!(report.enumerated, 3);
    assert_eq!(report.stubbed, 2);
    assert_eq!(report.schemas_written, 1);
    assert_eq!(report.queued_hydrations, 1);
    assert!(root.join("Home.md").exists());
    assert!(root.join("Home/Child.md").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("Home/Tasks ~tasks/_schema.yaml"))
            .expect("database schema"),
        "title: Tasks\nproperties: {}\n"
    );
    let root_stub = std::fs::read_to_string(root.join("Home.md")).expect("root stub");
    assert!(root_stub.contains(CanonicalDocument::STUB_MARKER));

    let entity = supervisor
        .store()
        .get_entity(&mount_id, &RemoteId::new("child-page"))
        .expect("get child entity")
        .expect("child entity");
    assert_eq!(entity.path, PathBuf::from("Home/Child.md"));
    assert_eq!(entity.hydration, HydrationState::Stub);

    let request = supervisor
        .hydration()
        .peek_ready()
        .expect("default root hydration request");
    assert_eq!(request.remote_id, RemoteId::new("root-page"));
    assert_eq!(request.reason, HydrationReason::Policy);
}

#[test]
fn scheduled_pull_macos_file_provider_keeps_unhydrated_pages_online_only() {
    assert_virtual_projection_keeps_unhydrated_pages_online_only(
        ProjectionMode::MacosFileProvider,
        "scheduled-pull-file-provider",
    );
}

#[test]
fn scheduled_pull_linux_fuse_keeps_unhydrated_pages_online_only() {
    assert_virtual_projection_keeps_unhydrated_pages_online_only(
        ProjectionMode::LinuxFuse,
        "scheduled-pull-linux-fuse",
    );
}

fn assert_virtual_projection_keeps_unhydrated_pages_online_only(
    projection: ProjectionMode,
    fixture_name: &str,
) {
    let root = temp_root(fixture_name);
    let mount_id = MountId::new("notion-main");
    let mount = MountConfig::new(mount_id.clone(), "notion", root.clone())
        .with_remote_root_id(RemoteId::new("root-page"))
        .projection(projection);
    let mut source = FakeScheduledPullSource::default();
    source.insert_entries(
        &mount_id,
        vec![
            page_entry(
                &mount_id,
                "root-page",
                "Home",
                "Home.md",
                "2026-06-10T00:00:00Z",
            ),
            page_entry(
                &mount_id,
                "child-page",
                "Child",
                "Home/Child.md",
                "2026-06-10T00:00:00Z",
            ),
            database_entry(&mount_id, "tasks-db", "Tasks", "Home/Tasks ~tasks"),
        ],
    );
    source.insert_schema("tasks-db", "title: Tasks\nproperties: {}\n");
    let mut supervisor = supervisor_with_mounts([mount]);

    supervisor.start().expect("start supervisor");
    let report = supervisor
        .advance_and_execute_scheduled_pull(
            AdvanceScheduledPullJob::new(Duration::ZERO),
            &source,
            &DefaultFetchScheduleStrategy,
        )
        .expect("scheduled pull");

    assert_eq!(report.enumerated, 3);
    assert_eq!(report.stubbed, 0);
    assert_eq!(report.schemas_written, 0);
    assert_eq!(report.queued_hydrations, 1);
    assert!(!root.join("Home.md").exists());
    assert!(!root.join("Home/Child.md").exists());
    assert!(!root.join("Home/Tasks ~tasks/_schema.yaml").exists());

    let child = supervisor
        .store()
        .get_entity(&mount_id, &RemoteId::new("child-page"))
        .expect("get child entity")
        .expect("child entity");
    assert_eq!(child.path, PathBuf::from("Home/Child.md"));
    assert_eq!(child.hydration, HydrationState::Stub);
}

#[test]
fn scheduled_pull_strategy_can_dispatch_per_mount() {
    let cold_root = temp_root("scheduled-pull-cold");
    let hot_root = temp_root("scheduled-pull-hot");
    let cold_mount_id = MountId::new("cold");
    let hot_mount_id = MountId::new("hot");
    let cold_mount = MountConfig::new(cold_mount_id.clone(), "notion", cold_root);
    let hot_mount = MountConfig::new(hot_mount_id.clone(), "notion", hot_root.clone());
    let mut source = FakeScheduledPullSource::default();
    source.insert_entries(
        &cold_mount_id,
        vec![page_entry(
            &cold_mount_id,
            "cold-page",
            "Cold",
            "Cold.md",
            "2026-06-10T00:00:00Z",
        )],
    );
    source.insert_entries(
        &hot_mount_id,
        vec![page_entry(
            &hot_mount_id,
            "hot-page",
            "Hot",
            "Hot.md",
            "2026-06-10T00:00:00Z",
        )],
    );
    let mut supervisor = supervisor_with_mounts([cold_mount, hot_mount]);

    supervisor.start().expect("start supervisor");
    let report = supervisor
        .advance_and_execute_scheduled_pull(
            AdvanceScheduledPullJob::new(Duration::ZERO),
            &source,
            &HotMountOnlyStrategy,
        )
        .expect("scheduled pull");

    assert_eq!(report.mounts_checked, 2);
    assert_eq!(report.mounts_polled, 1);
    assert_eq!(report.enumerated, 1);
    assert_eq!(report.stubbed, 1);
    assert_eq!(report.queued_hydrations, 1);
    assert_eq!(source.enumerated_mounts(), vec![hot_mount_id.clone()]);
    assert!(hot_root.join("Hot.md").exists());
    assert_eq!(
        supervisor
            .hydration()
            .peek_ready()
            .expect("hot hydration request")
            .remote_id,
        RemoteId::new("hot-page")
    );
}

#[test]
fn scheduled_pull_preserves_hydrated_files_and_queues_changed_remote_refresh() {
    let root = temp_root("scheduled-pull-preserve");
    let mount_id = MountId::new("notion-main");
    let mount = MountConfig::new(mount_id.clone(), "notion", root.clone());
    let mut store = InMemoryStateStore::new();
    store.save_mount(mount).expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(HydrationState::Hydrated)
            .with_remote_edited_at("2026-06-10T00:00:00Z"),
        )
        .expect("save existing entity");
    let mut supervisor = supervisor_with_store(store);
    std::fs::write(
        root.join("Roadmap.md"),
        "---\nafs:\n  id: page-1\n  type: page\n  synced_at: old\n  remote_edited_at: old\ntitle: Roadmap\n---\nLocal hydrated body.\n",
    )
    .expect("write hydrated file");
    let mut source = FakeScheduledPullSource::default();
    source.insert_entries(
        &mount_id,
        vec![page_entry(
            &mount_id,
            "page-1",
            "Roadmap",
            "Roadmap.md",
            "2026-06-11T00:00:00Z",
        )],
    );

    supervisor.start().expect("start supervisor");
    let report = supervisor
        .advance_and_execute_scheduled_pull(
            AdvanceScheduledPullJob::new(Duration::ZERO),
            &source,
            &DefaultFetchScheduleStrategy,
        )
        .expect("scheduled pull");

    assert_eq!(report.stubbed, 0);
    assert_eq!(report.queued_hydrations, 1);
    let contents = std::fs::read_to_string(root.join("Roadmap.md")).expect("hydrated file");
    assert!(contents.contains("Local hydrated body."));
    assert!(!contents.contains(CanonicalDocument::STUB_MARKER));
    let entity = supervisor
        .store()
        .get_entity(&mount_id, &RemoteId::new("page-1"))
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);
    assert_eq!(
        entity.remote_edited_at,
        Some("2026-06-10T00:00:00Z".to_string())
    );
}

#[test]
fn scheduled_pull_renames_existing_projection_when_remote_title_changes() {
    let root = temp_root("scheduled-pull-rename");
    let mount_id = MountId::new("notion-main");
    let mount = MountConfig::new(mount_id.clone(), "notion", root.clone())
        .with_remote_root_id(RemoteId::new("root-page"));
    let mut source = FakeScheduledPullSource::default();
    source.insert_entries(
        &mount_id,
        vec![
            page_entry(
                &mount_id,
                "root-page",
                "Home",
                "Home.md",
                "2026-06-10T00:00:00Z",
            ),
            page_entry(
                &mount_id,
                "child-page",
                "Child",
                "Home/Child.md",
                "2026-06-10T00:00:00Z",
            ),
        ],
    );
    let mut supervisor = supervisor_with_mounts([mount]);

    supervisor.start().expect("start supervisor");
    supervisor
        .advance_and_execute_scheduled_pull(
            AdvanceScheduledPullJob::new(Duration::ZERO),
            &source,
            &DefaultFetchScheduleStrategy,
        )
        .expect("initial scheduled pull");

    assert!(root.join("Home.md").exists());
    assert!(root.join("Home/Child.md").exists());

    source.insert_entries(
        &mount_id,
        vec![
            page_entry(
                &mount_id,
                "root-page",
                "Vision",
                "Vision.md",
                "2026-06-11T00:00:00Z",
            ),
            page_entry(
                &mount_id,
                "child-page",
                "Child",
                "Vision/Child.md",
                "2026-06-11T00:00:00Z",
            ),
        ],
    );

    supervisor
        .advance_and_execute_scheduled_pull(
            AdvanceScheduledPullJob::new(Duration::from_secs(15)),
            &source,
            &DefaultFetchScheduleStrategy,
        )
        .expect("rename scheduled pull");

    assert!(root.join("Vision.md").exists());
    assert!(root.join("Vision/Child.md").exists());
    assert!(!root.join("Home.md").exists());
    assert!(!root.join("Home/Child.md").exists());

    let root_entity = supervisor
        .store()
        .get_entity(&mount_id, &RemoteId::new("root-page"))
        .expect("get root entity")
        .expect("root entity");
    assert_eq!(root_entity.path, PathBuf::from("Vision.md"));
    let child_entity = supervisor
        .store()
        .get_entity(&mount_id, &RemoteId::new("child-page"))
        .expect("get child entity")
        .expect("child entity");
    assert_eq!(child_entity.path, PathBuf::from("Vision/Child.md"));
}

fn supervisor_with_mounts(
    mounts: impl IntoIterator<Item = MountConfig>,
) -> DaemonSupervisor<InMemoryStateStore, RecordingWatcher, HydrationQueue> {
    let mut store = InMemoryStateStore::new();
    for mount in mounts {
        store.save_mount(mount).expect("save mount");
    }

    supervisor_with_store(store)
}

fn supervisor_with_store(
    store: InMemoryStateStore,
) -> DaemonSupervisor<InMemoryStateStore, RecordingWatcher, HydrationQueue> {
    DaemonSupervisor::new(
        store,
        RecordingWatcher::default(),
        HydrationQueue::new(),
        PullScheduler::new(Default::default()),
    )
}

fn page_entry(
    mount_id: &MountId,
    remote_id: &str,
    title: &str,
    path: &str,
    remote_edited_at: &str,
) -> TreeEntry {
    TreeEntry {
        mount_id: mount_id.clone(),
        remote_id: RemoteId::new(remote_id),
        kind: EntityKind::Page,
        title: title.to_string(),
        path: PathBuf::from(path),
        hydration: HydrationState::Stub,
        content_hash: None,
        remote_edited_at: Some(remote_edited_at.to_string()),
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
    let root = std::env::temp_dir().join(format!("afs-{label}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp root");
    root
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RecordingWatcher {
    watched: Vec<PathBuf>,
}

impl FileWatcher for RecordingWatcher {
    fn watch_mount(&mut self, root: PathBuf) -> AfsResult<()> {
        self.watched.push(root);
        Ok(())
    }
}

#[derive(Default)]
struct FakeScheduledPullSource {
    entries: BTreeMap<MountId, Vec<TreeEntry>>,
    schemas: BTreeMap<RemoteId, String>,
    enumerated: RefCell<Vec<MountId>>,
}

impl FakeScheduledPullSource {
    fn insert_entries(&mut self, mount_id: &MountId, entries: Vec<TreeEntry>) {
        self.entries.insert(mount_id.clone(), entries);
    }

    fn insert_schema(&mut self, remote_id: &str, schema: &str) {
        self.schemas
            .insert(RemoteId::new(remote_id), schema.to_string());
    }

    fn enumerated_mounts(&self) -> Vec<MountId> {
        self.enumerated.borrow().clone()
    }
}

impl ScheduledPullSource for FakeScheduledPullSource {
    fn enumerate_mount(&self, mount: &MountConfig) -> AfsResult<Vec<TreeEntry>> {
        self.enumerated.borrow_mut().push(mount.mount_id.clone());
        self.entries.get(&mount.mount_id).cloned().ok_or_else(|| {
            AfsError::InvalidState(format!("missing fixture for {}", mount.mount_id.0))
        })
    }

    fn database_schema_yaml(
        &self,
        _mount: &MountConfig,
        remote_id: &RemoteId,
    ) -> AfsResult<Option<String>> {
        Ok(self.schemas.get(remote_id).cloned())
    }
}

struct HotMountOnlyStrategy;

impl FetchScheduleStrategy for HotMountOnlyStrategy {
    fn mount_plan(&self, request: MountFetchSchedule<'_>) -> MountFetchPlan {
        MountFetchPlan {
            enumerate: !request.tick.is_idle() && request.mount.mount_id == MountId::new("hot"),
        }
    }

    fn entity_plan(&self, request: EntityFetchSchedule<'_>) -> EntityFetchPlan {
        if request.entry.kind == EntityKind::Page {
            EntityFetchPlan {
                queue_hydration: Some(HydrationReason::Policy),
            }
        } else {
            EntityFetchPlan::default()
        }
    }
}
