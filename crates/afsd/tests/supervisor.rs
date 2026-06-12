use std::path::PathBuf;
use std::time::Duration;

use afs_core::hydration::{HydrationReason, HydrationRequest};
use afs_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use afs_core::shadow::ShadowDocument;
use afs_core::{AfsError, AfsResult};
use afs_store::{
    EntityRecord, EntityRepository, InMemoryStateStore, MountConfig, MountRepository,
    ProjectionMode, ShadowRepository,
};
use afsd::execution::{DaemonExecutor, HydrationDrainJob, HydrationRequestJob};
use afsd::hydration::{HydratedEntity, HydrationQueue, HydrationSource};
use afsd::scheduler::PullScheduler;
use afsd::supervisor::DaemonSupervisor;
use afsd::watcher::{FileEvent, FileEventKind, FileWatcher};

#[test]
fn supervisor_start_registers_mount_roots_with_watcher() {
    let mut supervisor = supervisor_with_entity(HydrationState::Stub);

    let report = supervisor.start().expect("start supervisor");

    assert_eq!(report.watched_mounts, 1);
    assert_eq!(
        supervisor.watcher().watched,
        vec![PathBuf::from("/tmp/afs/notion")]
    );
    assert_eq!(supervisor.mounts().len(), 1);
}

#[test]
fn supervisor_start_skips_virtual_projection_mount_roots() {
    let mut store = InMemoryStateStore::new();
    let plain_mount = MountConfig::new(
        MountId::new("notion-plain"),
        "notion",
        "/tmp/afs/notion-plain",
    );
    let virtual_mount = MountConfig::new(
        MountId::new("notion-fuse"),
        "notion",
        "/tmp/afs/notion-fuse",
    )
    .projection(ProjectionMode::LinuxFuse);
    store.save_mount(plain_mount).expect("save plain mount");
    store.save_mount(virtual_mount).expect("save virtual mount");
    let mut supervisor = DaemonSupervisor::new(
        store,
        RecordingWatcher::default(),
        HydrationQueue::new(),
        PullScheduler::new(Default::default()),
    );

    let report = supervisor.start().expect("start supervisor");

    assert_eq!(report.watched_mounts, 1);
    assert_eq!(
        supervisor.watcher().watched,
        vec![PathBuf::from("/tmp/afs/notion-plain")]
    );
    assert_eq!(supervisor.mounts().len(), 2);
}

#[test]
fn virtual_projection_file_event_is_ignored_by_host_watcher_path() {
    let mut store = InMemoryStateStore::new();
    let mount = MountConfig::new(
        MountId::new("notion-fuse"),
        "notion",
        "/tmp/afs/notion-fuse",
    )
    .projection(ProjectionMode::LinuxFuse);
    store.save_mount(mount.clone()).expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                mount.mount_id,
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(HydrationState::Stub),
        )
        .expect("save entity");
    let mut supervisor = DaemonSupervisor::new(
        store,
        RecordingWatcher::default(),
        HydrationQueue::new(),
        PullScheduler::new(Default::default()),
    );
    supervisor.start().expect("start supervisor");

    let report = supervisor
        .execute_file_event(FileEvent {
            path: PathBuf::from("/tmp/afs/notion-fuse/Roadmap.md"),
            kind: FileEventKind::Read,
        })
        .expect("handle event");

    assert_eq!(report.ignored_events, 1);
    assert!(supervisor.hydration().is_empty());
}

#[test]
fn read_event_on_stub_queues_hydration() {
    let mut supervisor = supervisor_with_entity(HydrationState::Stub);
    supervisor.start().expect("start supervisor");

    let report = supervisor
        .execute_file_event(read_event("/tmp/afs/notion/Roadmap.md"))
        .expect("handle read");

    assert_eq!(report.queued_hydrations, 1);
    let request = supervisor
        .hydration()
        .peek_ready()
        .expect("queued hydration");
    assert_eq!(request.mount_id, MountId::new("notion-main"));
    assert_eq!(request.remote_id, RemoteId::new("page-1"));
    assert_eq!(request.path, PathBuf::from("/tmp/afs/notion/Roadmap.md"));
    assert_eq!(request.reason, HydrationReason::StubRead);
    assert_eq!(request.target_state, HydrationState::Hydrated);
}

#[test]
fn read_event_on_hydrated_file_is_ignored() {
    let mut supervisor = supervisor_with_entity(HydrationState::Hydrated);
    supervisor.start().expect("start supervisor");

    let report = supervisor
        .execute_file_event(read_event("/tmp/afs/notion/Roadmap.md"))
        .expect("handle read");

    assert_eq!(report.ignored_events, 1);
    assert!(supervisor.hydration().is_empty());
}

#[test]
fn write_event_on_hydrated_file_marks_entity_dirty() {
    let mut supervisor = supervisor_with_entity(HydrationState::Hydrated);
    supervisor.start().expect("start supervisor");

    let report = supervisor
        .execute_file_event(FileEvent {
            path: PathBuf::from("/tmp/afs/notion/Roadmap.md"),
            kind: FileEventKind::Write,
        })
        .expect("handle write");

    assert_eq!(report.marked_dirty, 1);
    let entity = supervisor
        .store()
        .get_entity(&MountId::new("notion-main"), &RemoteId::new("page-1"))
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Dirty);
}

#[test]
fn scheduler_tick_is_exposed_through_supervisor() {
    let mut supervisor = supervisor_with_entity(HydrationState::Stub);

    let initial = supervisor
        .tick_scheduler(Duration::ZERO)
        .expect("initial scheduler tick");
    assert!(initial.poll_active);
    assert!(initial.poll_cold);

    let idle = supervisor
        .tick_scheduler(Duration::from_secs(1))
        .expect("idle scheduler tick");
    assert!(idle.is_idle());
}

#[test]
fn supervisor_drains_queued_hydration_through_source() {
    let _ = std::fs::remove_file("/tmp/afs/notion/Roadmap.md");
    let mut supervisor = supervisor_with_entity(HydrationState::Stub);
    supervisor.start().expect("start supervisor");
    supervisor
        .execute_file_event(read_event("/tmp/afs/notion/Roadmap.md"))
        .expect("handle read");

    let report = supervisor
        .execute_hydration_drain(HydrationDrainJob, &FakeHydrationSource)
        .expect("drain hydration");

    assert_eq!(report.hydrated, 1);
    let shadow = supervisor
        .store()
        .load_shadow(&MountId::new("notion-main"), &RemoteId::new("page-1"))
        .expect("load shadow");
    assert_eq!(shadow.entity_id, RemoteId::new("page-1"));
    let contents = std::fs::read_to_string("/tmp/afs/notion/Roadmap.md")
        .expect("hydrated file from supervisor");
    assert!(contents.contains("Hydrated from supervisor."));
    let _ = std::fs::remove_file("/tmp/afs/notion/Roadmap.md");
}

#[test]
fn supervisor_hydrates_single_request_through_daemon_job() {
    let root =
        std::env::temp_dir().join(format!("afs-supervisor-hydrate-job-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("test root");
    let page_path = root.join("Roadmap.md");
    let mut supervisor = supervisor_with_entity_at(root.clone(), HydrationState::Stub);
    supervisor.start().expect("start supervisor");

    let report = supervisor
        .execute_hydration_request(
            HydrationRequestJob::new(HydrationRequest::new(
                MountId::new("notion-main"),
                RemoteId::new("page-1"),
                page_path.clone(),
                HydrationState::Hydrated,
                HydrationReason::ExplicitPull,
            )),
            &FakeHydrationSource,
        )
        .expect("execute hydration job");

    assert_eq!(report.outcome, afsd::hydration::HydrationOutcome::Hydrated);
    let contents = std::fs::read_to_string(page_path).expect("hydrated file from daemon job");
    assert!(contents.contains("Hydrated from supervisor."));
    let _ = std::fs::remove_dir_all(root);
}

fn supervisor_with_entity(
    hydration: HydrationState,
) -> DaemonSupervisor<InMemoryStateStore, RecordingWatcher, HydrationQueue> {
    supervisor_with_entity_at(PathBuf::from("/tmp/afs/notion"), hydration)
}

fn supervisor_with_entity_at(
    root: PathBuf,
    hydration: HydrationState,
) -> DaemonSupervisor<InMemoryStateStore, RecordingWatcher, HydrationQueue> {
    let mut store = InMemoryStateStore::new();
    let mount = MountConfig::new(MountId::new("notion-main"), "notion", root);
    store.save_mount(mount.clone()).expect("save mount");
    store
        .save_entity(
            EntityRecord::new(
                mount.mount_id,
                RemoteId::new("page-1"),
                EntityKind::Page,
                "Roadmap",
                "Roadmap.md",
            )
            .with_hydration(hydration),
        )
        .expect("save entity");

    DaemonSupervisor::new(
        store,
        RecordingWatcher::default(),
        HydrationQueue::new(),
        PullScheduler::new(Default::default()),
    )
}

fn read_event(path: &str) -> FileEvent {
    FileEvent {
        path: PathBuf::from(path),
        kind: FileEventKind::Read,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RecordingWatcher {
    watched: Vec<PathBuf>,
}

impl FileWatcher for RecordingWatcher {
    fn watch_mount(&mut self, root: PathBuf) -> afs_core::AfsResult<()> {
        self.watched.push(root);
        Ok(())
    }
}

struct FakeHydrationSource;

impl HydrationSource for FakeHydrationSource {
    fn fetch_render(
        &self,
        request: &afs_core::hydration::HydrationRequest,
    ) -> AfsResult<HydratedEntity> {
        if request.remote_id != RemoteId::new("page-1") {
            return Err(AfsError::InvalidState("unexpected remote id".to_string()));
        }

        let body = "# Roadmap\n\nHydrated from supervisor.\n".to_string();
        let document = CanonicalDocument::new(
            "afs:\n  id: page-1\n  type: page\ntitle: Roadmap\n",
            body.clone(),
        );
        let shadow = ShadowDocument::from_synced_body(
            RemoteId::new("page-1"),
            body,
            7,
            [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
        )
        .expect("shadow");

        Ok(HydratedEntity {
            document,
            shadow,
            remote_edited_at: Some("2026-06-11T00:00:00Z".to_string()),
            assets: Vec::new(),
        })
    }
}
