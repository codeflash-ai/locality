use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use afs_core::AfsError;
use afs_core::canonical::render_canonical_markdown;
use afs_core::hydration::{HydrationPolicy, HydrationReason, HydrationRequest};
use afs_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
use afs_core::pull::PullMode;
use afs_core::shadow::ShadowDocument;
use afs_store::{
    EntityRecord, EntityRepository, MountConfig, MountRepository, ShadowRepository,
    SqliteStateStore,
};
use afsd::DaemonConfig;
use afsd::execution::{DaemonEventReport, PushJob};
use afsd::hydration::HydrationOutcome;
use afsd::ipc::{DaemonRequest, DaemonResponse};
use afsd::runtime::{
    DaemonRuntime, DefaultRuntimeJobRunner, FileEventRuntimeReport, RuntimeJobRunner,
    ScheduledPullRuntimeReport,
};
use afsd::scheduler::PullSchedulerTick;
use afsd::watcher::{FileEvent, FileEventKind};
use serde_json::json;

#[test]
fn runtime_answers_ping_while_pull_worker_is_blocked() {
    let (started_tx, started_rx) = mpsc::channel();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("ping-while-blocked"),
        BlockingPullRunner {
            started: started_tx,
            release: Arc::clone(&release),
        },
    )
    .expect("spawn runtime");
    let pull_handle = runtime.handle();

    let pull_thread = thread::spawn(move || {
        pull_handle.request(DaemonRequest::Pull {
            path: PathBuf::from("Roadmap.md"),
        })
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("pull started");

    let ping = runtime.handle().request(DaemonRequest::Ping);
    assert_eq!(ping, DaemonResponse::ok(json!({ "status": "ok" })));

    release_blocked_runner(&release);
    let pull = pull_thread.join().expect("pull thread");
    assert!(pull.ok);
    runtime.shutdown();
}

#[test]
fn runtime_serializes_mutating_requests() {
    let state = Arc::new(SerialState::default());
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("serial-mutating"),
        SerialRunner {
            state: Arc::clone(&state),
        },
    )
    .expect("spawn runtime");

    let first = runtime.handle();
    let first_thread = thread::spawn(move || {
        first.request(DaemonRequest::Pull {
            path: PathBuf::from("First.md"),
        })
    });
    let second = runtime.handle();
    let second_thread = thread::spawn(move || {
        second.request(DaemonRequest::Pull {
            path: PathBuf::from("Second.md"),
        })
    });

    state.wait_started(1);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(state.started_count(), 1);

    state.release(1);
    state.wait_started(2);
    assert_eq!(state.max_active.load(Ordering::SeqCst), 1);

    state.release(2);
    assert!(first_thread.join().expect("first response").ok);
    assert!(second_thread.join().expect("second response").ok);
    runtime.shutdown();
}

#[test]
fn runtime_scheduler_queues_and_drains_hydration() {
    let (scheduled_tx, scheduled_rx) = mpsc::channel();
    let (hydrated_tx, hydrated_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        polling_config("scheduled-hydration"),
        SchedulingRunner {
            scheduled: scheduled_tx,
            hydrated: hydrated_tx,
            scheduled_count: AtomicUsize::new(0),
        },
    )
    .expect("spawn runtime");

    scheduled_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("scheduled pull ran");
    let request = hydrated_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("hydration drained");

    assert_eq!(request.mount_id, MountId::new("notion-main"));
    assert_eq!(request.remote_id, RemoteId::new("page-1"));
    assert_eq!(request.reason, HydrationReason::Policy);
    runtime.shutdown();
}

#[test]
fn runtime_routes_file_events_through_worker_queue() {
    let (event_tx, event_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("file-event-routing"),
        EventRunner { event_tx },
    )
    .expect("spawn runtime");

    runtime
        .handle()
        .file_event(FileEvent {
            path: PathBuf::from("Roadmap.md"),
            kind: FileEventKind::Write,
        })
        .expect("submit file event");

    let event = event_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("file event ran");
    assert_eq!(event.path, PathBuf::from("Roadmap.md"));
    assert_eq!(event.kind, FileEventKind::Write);
    runtime.shutdown();
}

#[test]
fn default_runner_marks_hydrated_write_dirty() {
    let fixture = EventFixture::new("dirty-write");
    fixture.write_hydrated_page("Original body.");
    fixture.write_hydrated_page("Edited body.");

    let report = DefaultRuntimeJobRunner
        .run_file_event(fixture.state_root.clone(), fixture.write_event())
        .expect("run file event");

    assert_eq!(report.report.marked_dirty, 1);
    let store = SqliteStateStore::open(fixture.state_root).expect("open store");
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Dirty);
}

#[test]
fn default_runner_marks_frontmatter_only_write_dirty() {
    let fixture = EventFixture::new("frontmatter-write");
    fixture.write_hydrated_page("Original body.");
    fixture.write_hydrated_page_with_frontmatter(
        "afs:\n  id: page-1\n  type: page\ntitle: Updated Roadmap\n",
        "Original body.",
    );

    let report = DefaultRuntimeJobRunner
        .run_file_event(fixture.state_root.clone(), fixture.write_event())
        .expect("run file event");

    assert_eq!(report.report.marked_dirty, 1);
    let store = SqliteStateStore::open(fixture.state_root).expect("open store");
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Dirty);
}

#[test]
fn default_runner_ignores_clean_daemon_projection_write() {
    let fixture = EventFixture::new("clean-write");
    fixture.write_hydrated_page("Original body.");

    let report = DefaultRuntimeJobRunner
        .run_file_event(fixture.state_root.clone(), fixture.write_event())
        .expect("run file event");

    assert_eq!(report.report.ignored_events, 1);
    let store = SqliteStateStore::open(fixture.state_root).expect("open store");
    let entity = store
        .get_entity(&fixture.mount_id, &fixture.remote_id)
        .expect("get entity")
        .expect("entity");
    assert_eq!(entity.hydration, HydrationState::Hydrated);
}

#[test]
fn default_runner_queues_stub_read_hydration() {
    let fixture = EventFixture::new_with_state("stub-read", HydrationState::Stub);

    let report = DefaultRuntimeJobRunner
        .run_file_event(fixture.state_root.clone(), fixture.read_event())
        .expect("run file event");

    assert_eq!(report.report.queued_hydrations, 1);
    assert_eq!(report.queued_hydrations.len(), 1);
    let request = &report.queued_hydrations[0];
    assert_eq!(request.mount_id, fixture.mount_id);
    assert_eq!(request.remote_id, fixture.remote_id);
    assert_eq!(request.path, fixture.page_path());
    assert_eq!(request.target_state, HydrationState::Hydrated);
    assert_eq!(request.reason, HydrationReason::StubRead);
}

#[test]
fn default_runner_ignores_database_directory_read() {
    let fixture = EventFixture::new("database-read");
    let database_id = RemoteId::new("database-1");
    let database_path = PathBuf::from("Tasks");
    let mut store = SqliteStateStore::open(fixture.state_root.clone()).expect("open store");
    store
        .save_entity(
            EntityRecord::new(
                fixture.mount_id.clone(),
                database_id,
                EntityKind::Database,
                "Tasks",
                database_path.clone(),
            )
            .with_hydration(HydrationState::Stub),
        )
        .expect("save database entity");

    let report = DefaultRuntimeJobRunner
        .run_file_event(
            fixture.state_root.clone(),
            FileEvent {
                path: fixture.mount_root.join(database_path),
                kind: FileEventKind::Read,
            },
        )
        .expect("run file event");

    assert_eq!(report.report.ignored_events, 1);
    assert!(report.queued_hydrations.is_empty());
}

#[test]
fn runtime_drains_hydration_queued_by_read_event() {
    let (hydrated_tx, hydrated_rx) = mpsc::channel();
    let runtime = DaemonRuntime::spawn_with_runner(
        relay_config("read-event-hydration"),
        ReadHydrationRunner {
            hydrated: hydrated_tx,
        },
    )
    .expect("spawn runtime");

    runtime
        .handle()
        .file_event(FileEvent {
            path: PathBuf::from("Roadmap.md"),
            kind: FileEventKind::Read,
        })
        .expect("submit read event");

    let request = hydrated_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("hydration drained");
    assert_eq!(request.mount_id, MountId::new("notion-main"));
    assert_eq!(request.remote_id, RemoteId::new("page-1"));
    assert_eq!(request.reason, HydrationReason::StubRead);
    runtime.shutdown();
}

#[derive(Clone)]
struct BlockingPullRunner {
    started: mpsc::Sender<()>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl RuntimeJobRunner for BlockingPullRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        self.started.send(()).expect("notify started");
        let (lock, condvar) = &*self.release;
        let mut released = lock.lock().expect("lock release");
        while !*released {
            released = condvar.wait(released).expect("wait release");
        }
        DaemonResponse::ok(json!({ "command": "pull" }))
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> afs_core::AfsResult<ScheduledPullRuntimeReport> {
        Err(AfsError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> afs_core::AfsResult<HydrationOutcome> {
        Err(AfsError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }
}

#[derive(Default)]
struct SerialState {
    started: Mutex<usize>,
    started_condvar: Condvar,
    released: Mutex<usize>,
    released_condvar: Condvar,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl SerialState {
    fn mark_started(&self) -> usize {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.update_max_active(active);

        let mut started = self.started.lock().expect("started lock");
        *started += 1;
        let index = *started;
        self.started_condvar.notify_all();
        index
    }

    fn wait_started(&self, expected: usize) {
        let mut started = self.started.lock().expect("started lock");
        while *started < expected {
            started = self.started_condvar.wait(started).expect("wait started");
        }
    }

    fn started_count(&self) -> usize {
        *self.started.lock().expect("started lock")
    }

    fn release(&self, count: usize) {
        let mut released = self.released.lock().expect("released lock");
        *released = count;
        self.released_condvar.notify_all();
    }

    fn wait_released(&self, index: usize) {
        let mut released = self.released.lock().expect("released lock");
        while *released < index {
            released = self.released_condvar.wait(released).expect("wait released");
        }
    }

    fn mark_finished(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }

    fn update_max_active(&self, active: usize) {
        let mut current = self.max_active.load(Ordering::SeqCst);
        while active > current {
            match self.max_active.compare_exchange(
                current,
                active,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
    }
}

#[derive(Clone)]
struct SerialRunner {
    state: Arc<SerialState>,
}

impl RuntimeJobRunner for SerialRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        let index = self.state.mark_started();
        self.state.wait_released(index);
        self.state.mark_finished();
        DaemonResponse::ok(json!({ "command": "pull", "index": index }))
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> afs_core::AfsResult<ScheduledPullRuntimeReport> {
        Err(AfsError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> afs_core::AfsResult<HydrationOutcome> {
        Err(AfsError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }
}

struct SchedulingRunner {
    scheduled: mpsc::Sender<()>,
    hydrated: mpsc::Sender<HydrationRequest>,
    scheduled_count: AtomicUsize,
}

struct EventRunner {
    event_tx: mpsc::Sender<FileEvent>,
}

struct ReadHydrationRunner {
    hydrated: mpsc::Sender<HydrationRequest>,
}

impl RuntimeJobRunner for EventRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> afs_core::AfsResult<ScheduledPullRuntimeReport> {
        Err(AfsError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        _request: HydrationRequest,
    ) -> afs_core::AfsResult<HydrationOutcome> {
        Err(AfsError::InvalidState(
            "hydration should not run".to_string(),
        ))
    }

    fn run_file_event(
        &self,
        _state_root: PathBuf,
        event: FileEvent,
    ) -> afs_core::AfsResult<FileEventRuntimeReport> {
        self.event_tx.send(event).expect("send file event");
        Ok(FileEventRuntimeReport {
            report: DaemonEventReport {
                ignored_events: 1,
                ..Default::default()
            },
            queued_hydrations: Vec::new(),
        })
    }
}

impl RuntimeJobRunner for ReadHydrationRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> afs_core::AfsResult<ScheduledPullRuntimeReport> {
        Err(AfsError::InvalidState(
            "scheduled pull should not run".to_string(),
        ))
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        request: HydrationRequest,
    ) -> afs_core::AfsResult<HydrationOutcome> {
        self.hydrated.send(request).expect("notify hydrated");
        Ok(HydrationOutcome::Hydrated)
    }

    fn run_file_event(
        &self,
        _state_root: PathBuf,
        _event: FileEvent,
    ) -> afs_core::AfsResult<FileEventRuntimeReport> {
        Ok(FileEventRuntimeReport {
            report: DaemonEventReport {
                queued_hydrations: 1,
                ..Default::default()
            },
            queued_hydrations: vec![HydrationRequest::new(
                MountId::new("notion-main"),
                RemoteId::new("page-1"),
                PathBuf::from("Roadmap.md"),
                HydrationState::Hydrated,
                HydrationReason::StubRead,
            )],
        })
    }
}

impl RuntimeJobRunner for SchedulingRunner {
    fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
        DaemonResponse::error("unexpected_pull", "pull should not run")
    }

    fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
        DaemonResponse::error("unexpected_push", "push should not run")
    }

    fn run_scheduled_pull(
        &self,
        _state_root: PathBuf,
        _tick: PullSchedulerTick,
        _policy: HydrationPolicy,
    ) -> afs_core::AfsResult<ScheduledPullRuntimeReport> {
        self.scheduled.send(()).expect("notify scheduled");
        let queued_hydrations = if self.scheduled_count.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![HydrationRequest::new(
                MountId::new("notion-main"),
                RemoteId::new("page-1"),
                PathBuf::from("Roadmap.md"),
                HydrationState::Hydrated,
                HydrationReason::Policy,
            )]
        } else {
            Vec::new()
        };

        Ok(ScheduledPullRuntimeReport {
            report: Default::default(),
            queued_hydrations,
        })
    }

    fn run_hydration(
        &self,
        _state_root: PathBuf,
        request: HydrationRequest,
    ) -> afs_core::AfsResult<HydrationOutcome> {
        self.hydrated.send(request).expect("notify hydrated");
        Ok(HydrationOutcome::Hydrated)
    }
}

fn release_blocked_runner(release: &Arc<(Mutex<bool>, Condvar)>) {
    let (lock, condvar) = &**release;
    let mut released = lock.lock().expect("lock release");
    *released = true;
    condvar.notify_all();
}

fn relay_config(name: &str) -> DaemonConfig {
    let mut config = test_config(name);
    config.pull_scheduler.mode = PullMode::Relay;
    config
}

fn polling_config(name: &str) -> DaemonConfig {
    let mut config = test_config(name);
    config.pull_scheduler.mode = PullMode::Polling;
    config.pull_scheduler.active_interval = Duration::from_millis(5);
    config.pull_scheduler.cold_interval = Duration::from_millis(5);
    config.runtime_tick_interval = Duration::from_millis(5);
    config
}

fn test_config(name: &str) -> DaemonConfig {
    DaemonConfig {
        state_root: temp_root(name),
        runtime_tick_interval: Duration::from_millis(10),
        hydration_retry_delay: Duration::from_millis(25),
        ..Default::default()
    }
}

fn temp_root(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "afs-runtime-{name}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

struct EventFixture {
    state_root: PathBuf,
    mount_root: PathBuf,
    mount_id: MountId,
    remote_id: RemoteId,
}

impl EventFixture {
    fn new(name: &str) -> Self {
        Self::new_with_state(name, HydrationState::Hydrated)
    }

    fn new_with_state(name: &str, hydration: HydrationState) -> Self {
        let state_root = temp_root(&format!("{name}-state"));
        let mount_root = temp_root(&format!("{name}-mount"));
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let body = markdown_body("Original body.");
        let shadow = ShadowDocument::from_synced_body(
            remote_id.clone(),
            body,
            7,
            [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
        )
        .expect("shadow")
        .with_frontmatter(frontmatter());

        let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
        store
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                mount_root.clone(),
            ))
            .expect("save mount");
        store
            .save_shadow(&mount_id, shadow.clone())
            .expect("save shadow");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(hydration)
                .with_content_hash(shadow.body_hash),
            )
            .expect("save entity");

        Self {
            state_root,
            mount_root,
            mount_id,
            remote_id,
        }
    }

    fn page_path(&self) -> PathBuf {
        self.mount_root.join("Roadmap.md")
    }

    fn write_event(&self) -> FileEvent {
        FileEvent {
            path: self.page_path(),
            kind: FileEventKind::Write,
        }
    }

    fn read_event(&self) -> FileEvent {
        FileEvent {
            path: self.page_path(),
            kind: FileEventKind::Read,
        }
    }

    fn write_hydrated_page(&self, body: &str) {
        let document = CanonicalDocument::new(frontmatter(), markdown_body(body));
        std::fs::write(self.page_path(), render_canonical_markdown(&document)).expect("write page");
    }

    fn write_hydrated_page_with_frontmatter(&self, frontmatter: &str, body: &str) {
        let document = CanonicalDocument::new(frontmatter, markdown_body(body));
        std::fs::write(self.page_path(), render_canonical_markdown(&document)).expect("write page");
    }
}

fn frontmatter() -> String {
    "afs:\n  id: page-1\n  type: page\ntitle: Roadmap\n".to_string()
}

fn markdown_body(body: &str) -> String {
    format!("# Roadmap\n\n{body}\n")
}
