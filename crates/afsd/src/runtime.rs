//! Daemon runtime control loop.
//!
//! Socket handlers submit requests here instead of executing sync code directly.
//! The runtime keeps mutating work serialized while slow connector calls run on
//! worker threads, so health checks and future control-plane work stay
//! responsive during network I/O.

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use afs_core::AfsError;
use afs_core::canonical::parse_canonical_markdown;
use afs_core::hydration::{HydrationPolicy, HydrationReason, HydrationRequest};
use afs_core::model::{EntityKind, HydrationState, MountId};
use afs_core::pull::PullMode;
use afs_store::{
    EntityRecord, EntityRepository, HydrationJobRecord, HydrationJobRepository, MountConfig,
    MountRepository, ShadowRepository, SqliteStateStore, open_credential_store,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Serialize;
use serde_json::json;

use crate::DaemonConfig;
use crate::execution::{DaemonEventReport, PushJob};
use crate::hydration::{HydrationEngine, HydrationExecutor, HydrationOutcome, HydrationQueue};
use crate::ipc::{DaemonActiveJobStatus, DaemonRequest, DaemonResponse, DaemonRuntimeStatus};
use crate::pull::run_pull_with_state_root;
use crate::push::execute_push_job_with_content_root;
use crate::reconcile::{
    DefaultFetchScheduleStrategy, ScheduledPullReport, reconcile_scheduled_pull_with_state_root,
};
use crate::scheduler::{PullScheduler, PullSchedulerTick};
use crate::source::{ResolvedSourceSet, resolve_source_for_mount_id, resolve_source_for_path};
use crate::virtual_fs::{
    VirtualFsItem, VirtualFsMaterializeOutcome, commit_virtual_fs_write, create_virtual_fs_file,
    materialize_virtual_fs_item_with_content_root, refresh_virtual_fs_children,
    rename_virtual_fs_item, trash_virtual_fs_item, virtual_fs_children_refresh_needed,
    virtual_fs_children_with_content_root, virtual_fs_content_root,
    virtual_fs_item_with_content_root,
};
use crate::watcher::{FileEvent, FileEventKind};

#[derive(Clone)]
pub struct DaemonRuntimeHandle {
    sender: Sender<RuntimeMessage>,
}

const CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

impl DaemonRuntimeHandle {
    pub fn request(&self, request: DaemonRequest) -> DaemonResponse {
        let (respond_to, response) = mpsc::channel();
        if self
            .sender
            .send(RuntimeMessage::Request {
                request,
                respond_to,
            })
            .is_err()
        {
            return DaemonResponse::error("runtime_stopped", "daemon runtime is not running");
        }

        response.recv().unwrap_or_else(|_| {
            DaemonResponse::error(
                "runtime_stopped",
                "daemon runtime stopped before responding",
            )
        })
    }

    pub fn file_event(&self, event: FileEvent) -> Result<(), RuntimeSendError> {
        self.sender
            .send(RuntimeMessage::FileEvent(event))
            .map_err(|_| RuntimeSendError)
    }

    pub fn status(&self) -> Result<DaemonRuntimeStatus, RuntimeSendError> {
        let (respond_to, response) = mpsc::channel();
        self.sender
            .send(RuntimeMessage::Status { respond_to })
            .map_err(|_| RuntimeSendError)?;

        response
            .recv_timeout(CONTROL_RESPONSE_TIMEOUT)
            .map_err(|_| RuntimeSendError)
    }
}

#[derive(Debug)]
pub struct RuntimeSendError;

pub struct DaemonRuntime {
    handle: DaemonRuntimeHandle,
    join: Option<JoinHandle<()>>,
}

impl DaemonRuntime {
    pub fn spawn(config: DaemonConfig) -> afs_core::AfsResult<Self> {
        Self::spawn_with_runner(config, DefaultRuntimeJobRunner)
    }

    pub fn spawn_with_runner<Runner>(
        config: DaemonConfig,
        runner: Runner,
    ) -> afs_core::AfsResult<Self>
    where
        Runner: RuntimeJobRunner,
    {
        std::fs::create_dir_all(&config.state_root)?;
        SqliteStateStore::open(config.state_root.clone()).map_err(AfsError::from)?;
        let (sender, receiver) = mpsc::channel();
        let handle = DaemonRuntimeHandle {
            sender: sender.clone(),
        };
        let runner: Arc<dyn RuntimeJobRunner> = Arc::new(runner);
        let join = thread::spawn(move || RuntimeState::new(config, runner, sender).run(receiver));

        Ok(Self {
            handle,
            join: Some(join),
        })
    }

    pub fn handle(&self) -> DaemonRuntimeHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        let _ = self.handle.sender.send(RuntimeMessage::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for DaemonRuntime {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

pub trait RuntimeJobRunner: Send + Sync + 'static {
    fn run_pull(&self, state_root: PathBuf, path: PathBuf) -> DaemonResponse;

    fn run_push(&self, state_root: PathBuf, job: PushJob) -> DaemonResponse;

    fn run_scheduled_pull(
        &self,
        state_root: PathBuf,
        tick: PullSchedulerTick,
        policy: HydrationPolicy,
    ) -> afs_core::AfsResult<ScheduledPullRuntimeReport>;

    fn run_hydration(
        &self,
        state_root: PathBuf,
        request: HydrationRequest,
    ) -> afs_core::AfsResult<HydrationOutcome>;

    fn run_file_event(
        &self,
        _state_root: PathBuf,
        _event: FileEvent,
    ) -> afs_core::AfsResult<FileEventRuntimeReport> {
        Err(AfsError::Unsupported(
            "runtime runner does not handle file events",
        ))
    }

    fn run_virtual_fs_item(
        &self,
        _state_root: PathBuf,
        _mount_id: String,
        _identifier: String,
    ) -> DaemonResponse {
        DaemonResponse::error(
            "unsupported",
            "runtime runner does not handle virtual filesystem item metadata",
        )
    }

    fn run_virtual_fs_children(
        &self,
        _state_root: PathBuf,
        _mount_id: String,
        _container_identifier: String,
    ) -> DaemonResponse {
        DaemonResponse::error(
            "unsupported",
            "runtime runner does not handle virtual filesystem child enumeration",
        )
    }

    fn run_virtual_fs_refresh_children(
        &self,
        _state_root: PathBuf,
        _mount_id: String,
        _container_identifier: String,
    ) -> afs_core::AfsResult<usize> {
        Err(AfsError::Unsupported(
            "runtime runner does not handle virtual filesystem child refresh",
        ))
    }

    fn run_virtual_fs_materialize(
        &self,
        _state_root: PathBuf,
        _mount_id: String,
        _identifier: String,
    ) -> DaemonResponse {
        DaemonResponse::error(
            "unsupported",
            "runtime runner does not handle virtual filesystem materialization",
        )
    }

    fn run_virtual_fs_commit_write(
        &self,
        _state_root: PathBuf,
        _mount_id: String,
        _identifier: String,
        _contents_base64: String,
    ) -> DaemonResponse {
        DaemonResponse::error(
            "unsupported",
            "runtime runner does not handle virtual filesystem writes",
        )
    }

    fn run_virtual_fs_create_file(
        &self,
        _state_root: PathBuf,
        _mount_id: String,
        _parent_identifier: String,
        _filename: String,
    ) -> DaemonResponse {
        DaemonResponse::error(
            "unsupported",
            "runtime runner does not handle virtual filesystem creates",
        )
    }

    fn run_virtual_fs_rename(
        &self,
        _state_root: PathBuf,
        _mount_id: String,
        _identifier: String,
        _new_parent_identifier: String,
        _new_filename: String,
    ) -> DaemonResponse {
        DaemonResponse::error(
            "unsupported",
            "runtime runner does not handle virtual filesystem renames",
        )
    }

    fn run_virtual_fs_trash(
        &self,
        _state_root: PathBuf,
        _mount_id: String,
        _identifier: String,
    ) -> DaemonResponse {
        DaemonResponse::error(
            "unsupported",
            "runtime runner does not handle virtual filesystem deletes",
        )
    }

    fn run_file_provider_item(
        &self,
        state_root: PathBuf,
        mount_id: String,
        identifier: String,
    ) -> DaemonResponse {
        self.run_virtual_fs_item(state_root, mount_id, identifier)
    }

    fn run_file_provider_children(
        &self,
        state_root: PathBuf,
        mount_id: String,
        container_identifier: String,
    ) -> DaemonResponse {
        self.run_virtual_fs_children(state_root, mount_id, container_identifier)
    }

    fn run_file_provider_materialize(
        &self,
        state_root: PathBuf,
        mount_id: String,
        identifier: String,
    ) -> DaemonResponse {
        self.run_virtual_fs_materialize(state_root, mount_id, identifier)
    }

    fn run_file_provider_read(
        &self,
        _state_root: PathBuf,
        _mount_id: String,
        _identifier: String,
    ) -> DaemonResponse {
        DaemonResponse::error(
            "unsupported",
            "runtime runner does not handle File Provider file reads",
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileEventRuntimeReport {
    pub report: DaemonEventReport,
    pub queued_hydrations: Vec<HydrationRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledPullRuntimeReport {
    pub report: ScheduledPullReport,
    pub queued_hydrations: Vec<HydrationRequest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct FileProviderReadPayload {
    mount_id: String,
    identifier: String,
    remote_id: String,
    path: String,
    outcome: VirtualFsMaterializeOutcome,
    hydration: HydrationState,
    item: VirtualFsItem,
    contents_base64: String,
}

#[derive(Clone, Debug, Default)]
pub struct DefaultRuntimeJobRunner;

impl RuntimeJobRunner for DefaultRuntimeJobRunner {
    fn run_pull(&self, state_root: PathBuf, path: PathBuf) -> DaemonResponse {
        let mut store = match SqliteStateStore::open(state_root.clone()) {
            Ok(store) => store,
            Err(error) => {
                return DaemonResponse::error("store_open_failed", error.to_string());
            }
        };
        let credentials = open_credential_store(&state_root);
        let connector = match resolve_source_for_path(&store, credentials.as_ref(), &path) {
            Ok(connector) => connector,
            Err(error) => return DaemonResponse::error(error.code(), error.message()),
        };

        match run_pull_with_state_root(&mut store, &connector, path, Some(&state_root)) {
            Ok(mut report) => {
                report.via = "daemon".to_string();
                DaemonResponse::ok(report)
            }
            Err(error) => DaemonResponse::error(error.code(), error.message()),
        }
    }

    fn run_push(&self, state_root: PathBuf, job: PushJob) -> DaemonResponse {
        let mut store = match SqliteStateStore::open(state_root.clone()) {
            Ok(store) => store,
            Err(error) => {
                return DaemonResponse::error("store_open_failed", error.to_string());
            }
        };
        let credentials = open_credential_store(&state_root);
        let connector =
            match resolve_source_for_path(&store, credentials.as_ref(), &job.target_path) {
                Ok(connector) => connector,
                Err(error) => return DaemonResponse::error(error.code(), error.message()),
            };

        match execute_push_job_with_content_root(&mut store, job, &connector, Some(&state_root)) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(afs_error_code(&error), error.to_string()),
        }
    }

    fn run_scheduled_pull(
        &self,
        state_root: PathBuf,
        tick: PullSchedulerTick,
        policy: HydrationPolicy,
    ) -> afs_core::AfsResult<ScheduledPullRuntimeReport> {
        let mut store = SqliteStateStore::open(state_root.clone()).map_err(AfsError::from)?;
        let mounts = store.load_mounts().map_err(AfsError::from)?;
        let credentials = open_credential_store(&state_root);
        let source = ResolvedSourceSet::new(&store, credentials.as_ref(), &mounts)
            .map_err(AfsError::from)?;
        let mut hydration = HydrationCollector::default();
        let report = reconcile_scheduled_pull_with_state_root(
            &mut store,
            &mut hydration,
            &mounts,
            &tick,
            &source,
            &DefaultFetchScheduleStrategy,
            &policy,
            Some(&state_root),
        )?;

        Ok(ScheduledPullRuntimeReport {
            report,
            queued_hydrations: hydration.into_requests(),
        })
    }

    fn run_hydration(
        &self,
        state_root: PathBuf,
        request: HydrationRequest,
    ) -> afs_core::AfsResult<HydrationOutcome> {
        let mut store = SqliteStateStore::open(state_root.clone()).map_err(AfsError::from)?;
        let request = hydration_request_for_projection(&store, &state_root, request)?;
        let credentials = open_credential_store(&state_root);
        let connector =
            resolve_source_for_mount_id(&store, credentials.as_ref(), &request.mount_id)
                .map_err(AfsError::from)?;
        let output_root = hydration_output_root_for_projection(&store, &state_root, &request)?;
        let mut executor = if let Some(output_root) = output_root {
            HydrationExecutor::new_with_output_root(&mut store, &connector, output_root)
        } else {
            HydrationExecutor::new(&mut store, &connector)
        };
        executor.hydrate_request(request)
    }

    fn run_file_event(
        &self,
        state_root: PathBuf,
        event: FileEvent,
    ) -> afs_core::AfsResult<FileEventRuntimeReport> {
        let mut store = SqliteStateStore::open(state_root).map_err(AfsError::from)?;
        execute_file_event(&mut store, event)
    }

    fn run_virtual_fs_item(
        &self,
        state_root: PathBuf,
        mount_id: String,
        identifier: String,
    ) -> DaemonResponse {
        let store = match SqliteStateStore::open(state_root.clone()) {
            Ok(store) => store,
            Err(error) => return DaemonResponse::error("store_open_failed", error.to_string()),
        };
        let mount_id = MountId::new(mount_id);
        if let Some(response) = reject_plain_files_virtual_fs_mount(&store, &mount_id) {
            return response;
        }
        let content_root = virtual_fs_content_root(&state_root, &mount_id);
        match virtual_fs_item_with_content_root(&store, &content_root, &mount_id, &identifier) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(afs_error_code(&error), error.to_string()),
        }
    }

    fn run_virtual_fs_children(
        &self,
        state_root: PathBuf,
        mount_id: String,
        container_identifier: String,
    ) -> DaemonResponse {
        let store = match SqliteStateStore::open(state_root.clone()) {
            Ok(store) => store,
            Err(error) => return DaemonResponse::error("store_open_failed", error.to_string()),
        };
        let mount_id = MountId::new(mount_id);
        if let Some(response) = reject_plain_files_virtual_fs_mount(&store, &mount_id) {
            return response;
        }
        let content_root = virtual_fs_content_root(&state_root, &mount_id);
        match virtual_fs_children_with_content_root(
            &store,
            &content_root,
            &mount_id,
            &container_identifier,
        ) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(afs_error_code(&error), error.to_string()),
        }
    }

    fn run_virtual_fs_refresh_children(
        &self,
        state_root: PathBuf,
        mount_id: String,
        container_identifier: String,
    ) -> afs_core::AfsResult<usize> {
        let mut store = SqliteStateStore::open(state_root.clone()).map_err(AfsError::from)?;
        let mount_id = MountId::new(mount_id);
        ensure_virtual_fs_mount(&store, &mount_id)?;
        if !virtual_fs_children_refresh_needed(&store, &mount_id, &container_identifier)? {
            return Ok(0);
        }
        let credentials = open_credential_store(&state_root);
        let connector = resolve_source_for_mount_id(&store, credentials.as_ref(), &mount_id)
            .map_err(AfsError::from)?;
        refresh_virtual_fs_children(&mut store, &connector, &mount_id, &container_identifier)
    }

    fn run_virtual_fs_materialize(
        &self,
        state_root: PathBuf,
        mount_id: String,
        identifier: String,
    ) -> DaemonResponse {
        let mut store = match SqliteStateStore::open(state_root.clone()) {
            Ok(store) => store,
            Err(error) => return DaemonResponse::error("store_open_failed", error.to_string()),
        };
        let mount_id = MountId::new(mount_id);
        if let Some(response) = reject_plain_files_virtual_fs_mount(&store, &mount_id) {
            return response;
        }
        let credentials = open_credential_store(&state_root);
        let connector = match resolve_source_for_mount_id(&store, credentials.as_ref(), &mount_id) {
            Ok(connector) => connector,
            Err(error) => return DaemonResponse::error(error.code(), error.message()),
        };
        let content_root = virtual_fs_content_root(&state_root, &mount_id);
        match materialize_virtual_fs_item_with_content_root(
            &mut store,
            &connector,
            &content_root,
            &mount_id,
            &identifier,
        ) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(afs_error_code(&error), error.to_string()),
        }
    }

    fn run_file_provider_read(
        &self,
        state_root: PathBuf,
        mount_id: String,
        identifier: String,
    ) -> DaemonResponse {
        let mut store = match SqliteStateStore::open(state_root.clone()) {
            Ok(store) => store,
            Err(error) => return DaemonResponse::error("store_open_failed", error.to_string()),
        };
        let mount_id = MountId::new(mount_id);
        if let Some(response) = reject_plain_files_virtual_fs_mount(&store, &mount_id) {
            return response;
        }
        let credentials = open_credential_store(&state_root);
        let connector = match resolve_source_for_mount_id(&store, credentials.as_ref(), &mount_id) {
            Ok(connector) => connector,
            Err(error) => return DaemonResponse::error(error.code(), error.message()),
        };
        let content_root = virtual_fs_content_root(&state_root, &mount_id);
        let materialized = match materialize_virtual_fs_item_with_content_root(
            &mut store,
            &connector,
            &content_root,
            &mount_id,
            &identifier,
        ) {
            Ok(report) => report,
            Err(error) => return DaemonResponse::error(afs_error_code(&error), error.to_string()),
        };
        let contents = match std::fs::read(&materialized.path) {
            Ok(contents) => contents,
            Err(error) => return DaemonResponse::error("read_failed", error.to_string()),
        };
        let item = match virtual_fs_item_with_content_root(
            &store,
            &content_root,
            &mount_id,
            &identifier,
        ) {
            Ok(report) => report.item,
            Err(error) => return DaemonResponse::error(afs_error_code(&error), error.to_string()),
        };

        DaemonResponse::ok(FileProviderReadPayload {
            mount_id: materialized.mount_id,
            identifier: materialized.identifier,
            remote_id: materialized.remote_id,
            path: materialized.path,
            outcome: materialized.outcome,
            hydration: materialized.hydration,
            item,
            contents_base64: BASE64.encode(contents),
        })
    }

    fn run_virtual_fs_commit_write(
        &self,
        state_root: PathBuf,
        mount_id: String,
        identifier: String,
        contents_base64: String,
    ) -> DaemonResponse {
        let mut store = match SqliteStateStore::open(state_root.clone()) {
            Ok(store) => store,
            Err(error) => return DaemonResponse::error("store_open_failed", error.to_string()),
        };
        let contents = match BASE64.decode(contents_base64.as_bytes()) {
            Ok(contents) => contents,
            Err(error) => return DaemonResponse::error("invalid_base64", error.to_string()),
        };
        let mount_id = MountId::new(mount_id);
        if let Some(response) = reject_plain_files_virtual_fs_mount(&store, &mount_id) {
            return response;
        }
        let content_root = virtual_fs_content_root(&state_root, &mount_id);
        match commit_virtual_fs_write(&mut store, &content_root, &mount_id, &identifier, &contents)
        {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(afs_error_code(&error), error.to_string()),
        }
    }

    fn run_virtual_fs_create_file(
        &self,
        state_root: PathBuf,
        mount_id: String,
        parent_identifier: String,
        filename: String,
    ) -> DaemonResponse {
        let mut store = match SqliteStateStore::open(state_root.clone()) {
            Ok(store) => store,
            Err(error) => return DaemonResponse::error("store_open_failed", error.to_string()),
        };
        let mount_id = MountId::new(mount_id);
        if let Some(response) = reject_plain_files_virtual_fs_mount(&store, &mount_id) {
            return response;
        }
        let content_root = virtual_fs_content_root(&state_root, &mount_id);
        match create_virtual_fs_file(
            &mut store,
            &content_root,
            &mount_id,
            &parent_identifier,
            &filename,
        ) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(afs_error_code(&error), error.to_string()),
        }
    }

    fn run_virtual_fs_rename(
        &self,
        state_root: PathBuf,
        mount_id: String,
        identifier: String,
        new_parent_identifier: String,
        new_filename: String,
    ) -> DaemonResponse {
        let mut store = match SqliteStateStore::open(state_root.clone()) {
            Ok(store) => store,
            Err(error) => return DaemonResponse::error("store_open_failed", error.to_string()),
        };
        let mount_id = MountId::new(mount_id);
        if let Some(response) = reject_plain_files_virtual_fs_mount(&store, &mount_id) {
            return response;
        }
        let content_root = virtual_fs_content_root(&state_root, &mount_id);
        match rename_virtual_fs_item(
            &mut store,
            &content_root,
            &mount_id,
            &identifier,
            &new_parent_identifier,
            &new_filename,
        ) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(afs_error_code(&error), error.to_string()),
        }
    }

    fn run_virtual_fs_trash(
        &self,
        state_root: PathBuf,
        mount_id: String,
        identifier: String,
    ) -> DaemonResponse {
        let mut store = match SqliteStateStore::open(state_root.clone()) {
            Ok(store) => store,
            Err(error) => return DaemonResponse::error("store_open_failed", error.to_string()),
        };
        let mount_id = MountId::new(mount_id);
        if let Some(response) = reject_plain_files_virtual_fs_mount(&store, &mount_id) {
            return response;
        }
        let content_root = virtual_fs_content_root(&state_root, &mount_id);
        match trash_virtual_fs_item(&mut store, &content_root, &mount_id, &identifier) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(afs_error_code(&error), error.to_string()),
        }
    }
}

fn reject_plain_files_virtual_fs_mount<S>(store: &S, mount_id: &MountId) -> Option<DaemonResponse>
where
    S: MountRepository,
{
    match store.get_mount(mount_id) {
        Ok(Some(mount)) if !mount.projection.uses_virtual_filesystem() => {
            let error = AfsError::Unsupported(
                "plain-files mounts do not support virtual filesystem operations",
            );
            Some(DaemonResponse::error(
                afs_error_code(&error),
                error.to_string(),
            ))
        }
        Ok(_) => None,
        Err(error) => Some(DaemonResponse::error("store_error", error.to_string())),
    }
}

fn ensure_virtual_fs_mount<S>(store: &S, mount_id: &MountId) -> afs_core::AfsResult<()>
where
    S: MountRepository,
{
    match store.get_mount(mount_id).map_err(AfsError::from)? {
        Some(mount) if !mount.projection.uses_virtual_filesystem() => Err(AfsError::Unsupported(
            "plain-files mounts do not support virtual filesystem operations",
        )),
        _ => Ok(()),
    }
}

#[derive(Default)]
struct HydrationCollector {
    requests: Vec<HydrationRequest>,
}

impl HydrationCollector {
    fn into_requests(self) -> Vec<HydrationRequest> {
        self.requests
    }
}

impl HydrationEngine for HydrationCollector {
    fn queue(&mut self, request: HydrationRequest) -> afs_core::AfsResult<()> {
        self.requests.push(request);
        Ok(())
    }

    fn drain_ready(&mut self) -> afs_core::AfsResult<usize> {
        let count = self.requests.len();
        self.requests.clear();
        Ok(count)
    }
}

struct RuntimeState {
    config: DaemonConfig,
    runner: Arc<dyn RuntimeJobRunner>,
    sender: Sender<RuntimeMessage>,
    pending_requests: VecDeque<MutatingRequest>,
    pending_child_refreshes: BTreeSet<(String, String)>,
    hydration: HydrationQueue,
    deferred_hydration: Vec<HydrationRequest>,
    next_hydration_retry: Option<Instant>,
    pending_scheduled_tick: Option<PullSchedulerTick>,
    scheduler: PullScheduler,
    last_scheduler_advance: Instant,
    active_job: Option<ActiveRuntimeJob>,
}

#[derive(Clone, Debug)]
struct ActiveRuntimeJob {
    kind: String,
    target: Option<String>,
    started_at: Instant,
    started_at_unix_ms: u64,
}

impl ActiveRuntimeJob {
    fn from_job(job: &MutatingJob) -> Self {
        let (kind, target) = job.active_status_parts();
        Self {
            kind,
            target,
            started_at: Instant::now(),
            started_at_unix_ms: unix_time_ms(),
        }
    }

    fn status(&self) -> DaemonActiveJobStatus {
        DaemonActiveJobStatus {
            kind: self.kind.clone(),
            target: self.target.clone(),
            elapsed_ms: self
                .started_at
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            started_at_unix_ms: self.started_at_unix_ms,
        }
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

impl RuntimeState {
    fn new(
        config: DaemonConfig,
        runner: Arc<dyn RuntimeJobRunner>,
        sender: Sender<RuntimeMessage>,
    ) -> Self {
        let hydration = load_persisted_hydrations(&config.state_root);

        Self {
            scheduler: PullScheduler::new(config.pull_scheduler.clone()),
            last_scheduler_advance: Instant::now(),
            config,
            runner,
            sender,
            pending_requests: VecDeque::new(),
            pending_child_refreshes: BTreeSet::new(),
            hydration,
            deferred_hydration: Vec::new(),
            next_hydration_retry: None,
            pending_scheduled_tick: None,
            active_job: None,
        }
    }

    fn run(mut self, receiver: Receiver<RuntimeMessage>) {
        loop {
            match receiver.recv_timeout(self.config.runtime_tick_interval) {
                Ok(RuntimeMessage::Request {
                    request,
                    respond_to,
                }) => self.handle_request(request, respond_to),
                Ok(RuntimeMessage::FileEvent(event)) => self.handle_file_event(event),
                Ok(RuntimeMessage::Status { respond_to }) => {
                    let _ = respond_to.send(self.status());
                }
                Ok(RuntimeMessage::JobFinished(completion)) => self.handle_completion(completion),
                Ok(RuntimeMessage::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => self.handle_timeout(),
            }
        }
    }

    fn handle_request(&mut self, request: DaemonRequest, respond_to: Sender<DaemonResponse>) {
        match request {
            DaemonRequest::Ping => {
                let _ = respond_to.send(DaemonResponse::ok(json!({ "status": "ok" })));
            }
            DaemonRequest::Status => {
                let _ = respond_to.send(DaemonResponse::ok(self.status()));
            }
            DaemonRequest::ReloadMounts => {
                let _ = respond_to.send(DaemonResponse::error(
                    "unsupported",
                    "mount reload is handled by the daemon server",
                ));
            }
            DaemonRequest::Pull { path } => {
                self.pending_requests
                    .push_back(MutatingRequest::Pull { path, respond_to });
                self.maybe_start_next_job();
            }
            DaemonRequest::Push {
                path,
                assume_yes,
                confirm_dangerous,
            } => {
                self.pending_requests.push_back(MutatingRequest::Push {
                    job: PushJob {
                        target_path: path,
                        assume_yes,
                        confirm_dangerous,
                    },
                    respond_to,
                });
                self.maybe_start_next_job();
            }
            DaemonRequest::VirtualFsItem {
                mount_id,
                identifier,
            }
            | DaemonRequest::FileProviderItem {
                mount_id,
                identifier,
            } => {
                let response = self.runner.run_virtual_fs_item(
                    self.config.state_root.clone(),
                    mount_id,
                    identifier,
                );
                let _ = respond_to.send(response);
            }
            DaemonRequest::VirtualFsChildren {
                mount_id,
                container_identifier,
            }
            | DaemonRequest::FileProviderChildren {
                mount_id,
                container_identifier,
            } => {
                let response = self.runner.run_virtual_fs_children(
                    self.config.state_root.clone(),
                    mount_id.clone(),
                    container_identifier.clone(),
                );
                let should_refresh = response.ok;
                let _ = respond_to.send(response);
                if should_refresh {
                    self.queue_child_refresh(mount_id, container_identifier);
                }
            }
            DaemonRequest::VirtualFsMaterialize {
                mount_id,
                identifier,
            }
            | DaemonRequest::FileProviderMaterialize {
                mount_id,
                identifier,
            } => {
                self.pending_requests
                    .push_front(MutatingRequest::VirtualFsMaterialize {
                        mount_id,
                        identifier,
                        respond_to,
                    });
                self.maybe_start_next_job();
            }
            DaemonRequest::FileProviderRead {
                mount_id,
                identifier,
            } => {
                self.pending_requests
                    .push_front(MutatingRequest::FileProviderRead {
                        mount_id,
                        identifier,
                        respond_to,
                    });
                self.maybe_start_next_job();
            }
            DaemonRequest::VirtualFsCommitWrite {
                mount_id,
                identifier,
                contents_base64,
            } => {
                self.pending_requests
                    .push_front(MutatingRequest::VirtualFsCommitWrite {
                        mount_id,
                        identifier,
                        contents_base64,
                        respond_to,
                    });
                self.maybe_start_next_job();
            }
            DaemonRequest::VirtualFsCreateFile {
                mount_id,
                parent_identifier,
                filename,
            } => {
                self.pending_requests
                    .push_front(MutatingRequest::VirtualFsCreateFile {
                        mount_id,
                        parent_identifier,
                        filename,
                        respond_to,
                    });
                self.maybe_start_next_job();
            }
            DaemonRequest::VirtualFsRename {
                mount_id,
                identifier,
                new_parent_identifier,
                new_filename,
            } => {
                self.pending_requests
                    .push_front(MutatingRequest::VirtualFsRename {
                        mount_id,
                        identifier,
                        new_parent_identifier,
                        new_filename,
                        respond_to,
                    });
                self.maybe_start_next_job();
            }
            DaemonRequest::VirtualFsTrash {
                mount_id,
                identifier,
            } => {
                self.pending_requests
                    .push_front(MutatingRequest::VirtualFsTrash {
                        mount_id,
                        identifier,
                        respond_to,
                    });
                self.maybe_start_next_job();
            }
        }
    }

    fn handle_file_event(&mut self, event: FileEvent) {
        self.pending_requests
            .push_back(MutatingRequest::FileEvent { event });
        self.maybe_start_next_job();
    }

    fn handle_completion(&mut self, completion: JobCompletion) {
        self.active_job = None;

        match completion {
            JobCompletion::Pull {
                response,
                respond_to,
            }
            | JobCompletion::Push {
                response,
                respond_to,
            }
            | JobCompletion::Response {
                response,
                respond_to,
            } => {
                let _ = respond_to.send(response);
            }
            JobCompletion::VirtualFsRefreshChildren {
                mount_id,
                container_identifier,
                result,
            } => {
                self.pending_child_refreshes
                    .remove(&(mount_id.clone(), container_identifier.clone()));
                if let Err(error) = result {
                    eprintln!(
                        "afsd virtual filesystem child refresh failed for `{mount_id}:{container_identifier}`: {error}"
                    );
                }
            }
            JobCompletion::ScheduledPull(result) => match result {
                Ok(result) => {
                    for request in result.queued_hydrations {
                        self.queue_hydration(request);
                    }
                }
                Err(error) => eprintln!("afsd scheduled pull failed: {error}"),
            },
            JobCompletion::Hydration { request, result } => match result {
                Ok(_) => self.delete_hydration_job(&request),
                Err(error) => {
                    eprintln!(
                        "afsd hydration failed for `{}`: {error}",
                        request.path.display()
                    );
                    self.record_hydration_failure(&request, error.to_string());
                    self.defer_hydration_retry(request);
                }
            },
            JobCompletion::FileEvent(result) => match result {
                Ok(result) => {
                    for request in result.queued_hydrations {
                        self.queue_hydration(request);
                    }
                }
                Err(error) => eprintln!("afsd file event failed: {error}"),
            },
        }

        self.maybe_start_next_job();
    }

    fn handle_timeout(&mut self) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last_scheduler_advance);
        self.last_scheduler_advance = now;

        if self
            .next_hydration_retry
            .is_some_and(|retry_at| now >= retry_at)
        {
            let retry_requests = std::mem::take(&mut self.deferred_hydration);
            for request in retry_requests {
                self.queue_hydration(request);
            }
            self.next_hydration_retry = None;
        }

        match self.scheduler.advance_by(elapsed) {
            Ok(tick) if !tick.is_idle() => self.merge_scheduled_tick(tick),
            Ok(_) => {}
            Err(error) => eprintln!("afsd scheduler tick failed: {error}"),
        }

        self.maybe_start_next_job();
    }

    fn maybe_start_next_job(&mut self) {
        if self.active_job.is_some() {
            return;
        }

        let job = if let Some(request) = self.pending_requests.pop_front() {
            Some(MutatingJob::Request(request))
        } else if let Some(request) = self.hydration.pop_ready() {
            Some(MutatingJob::Hydration { request })
        } else {
            self.pending_scheduled_tick
                .take()
                .map(|tick| MutatingJob::ScheduledPull { tick })
        };

        let Some(job) = job else {
            return;
        };

        self.active_job = Some(ActiveRuntimeJob::from_job(&job));
        let sender = self.sender.clone();
        let runner = Arc::clone(&self.runner);
        let state_root = self.config.state_root.clone();
        let policy = self.config.pull_scheduler.hydration_policy.clone();

        thread::spawn(move || {
            let completion = run_job(runner, state_root, policy, job);
            let _ = sender.send(RuntimeMessage::JobFinished(completion));
        });
    }

    fn merge_scheduled_tick(&mut self, tick: PullSchedulerTick) {
        match &mut self.pending_scheduled_tick {
            Some(pending) => {
                pending.poll_active |= tick.poll_active;
                pending.poll_cold |= tick.poll_cold;
            }
            None => self.pending_scheduled_tick = Some(tick),
        }
    }

    fn defer_hydration_retry(&mut self, request: HydrationRequest) {
        self.deferred_hydration.push(request);
        let retry_at = Instant::now() + self.config.hydration_retry_delay;
        self.next_hydration_retry = Some(
            self.next_hydration_retry
                .map_or(retry_at, |current| current.min(retry_at)),
        );
    }

    fn queue_hydration(&mut self, request: HydrationRequest) {
        self.hydration.queue_request(request.clone());

        match SqliteStateStore::open(self.config.state_root.clone())
            .and_then(|mut store| store.upsert_hydration_job(HydrationJobRecord::from(request)))
        {
            Ok(()) => {}
            Err(error) => eprintln!("afsd failed to persist hydration request: {error}"),
        }
    }

    fn queue_child_refresh(&mut self, mount_id: String, container_identifier: String) {
        let key = (mount_id.clone(), container_identifier.clone());
        if !self.pending_child_refreshes.insert(key) {
            return;
        }
        self.pending_requests
            .push_back(MutatingRequest::VirtualFsRefreshChildren {
                mount_id,
                container_identifier,
            });
        self.maybe_start_next_job();
    }

    fn delete_hydration_job(&self, request: &HydrationRequest) {
        match SqliteStateStore::open(self.config.state_root.clone())
            .and_then(|mut store| store.delete_hydration_job(&request.mount_id, &request.remote_id))
        {
            Ok(()) => {}
            Err(error) => eprintln!("afsd failed to remove completed hydration request: {error}"),
        }
    }

    fn record_hydration_failure(&self, request: &HydrationRequest, message: String) {
        match SqliteStateStore::open(self.config.state_root.clone()).and_then(|mut store| {
            store.record_hydration_job_failure(&request.mount_id, &request.remote_id, message)
        }) {
            Ok(()) => {}
            Err(error) => eprintln!("afsd failed to record hydration failure: {error}"),
        }
    }

    fn status(&self) -> DaemonRuntimeStatus {
        DaemonRuntimeStatus {
            active_job: self.active_job.is_some(),
            active_job_detail: self.active_job.as_ref().map(ActiveRuntimeJob::status),
            pending_requests: self.pending_requests.len(),
            pending_hydrations: self.hydration.len(),
            deferred_hydrations: self.deferred_hydration.len(),
            pending_scheduled_pull: self.pending_scheduled_tick.is_some(),
            scheduler_mode: match self.scheduler.config.mode {
                PullMode::Polling => "polling",
                PullMode::Relay => "relay",
            }
            .to_string(),
            active_interval_ms: self
                .scheduler
                .config
                .active_interval
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            cold_interval_ms: self
                .scheduler
                .config
                .cold_interval
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        }
    }
}

fn load_persisted_hydrations(state_root: &Path) -> HydrationQueue {
    let mut queue = HydrationQueue::new();
    match SqliteStateStore::open(state_root.to_path_buf())
        .and_then(|store| store.list_hydration_jobs())
    {
        Ok(jobs) => {
            for job in jobs {
                queue.queue_request(job.into_request());
            }
        }
        Err(error) => eprintln!("afsd failed to load persisted hydration requests: {error}"),
    }

    queue
}

fn hydration_request_for_projection<S>(
    store: &S,
    state_root: &Path,
    request: HydrationRequest,
) -> afs_core::AfsResult<HydrationRequest>
where
    S: MountRepository + EntityRepository,
{
    let Some(mount) = store.get_mount(&request.mount_id).map_err(AfsError::from)? else {
        return Ok(request);
    };
    if !mount.projection.uses_virtual_filesystem() {
        return Ok(request);
    }
    let Some(entity) = store
        .get_entity(&request.mount_id, &request.remote_id)
        .map_err(AfsError::from)?
    else {
        return Ok(request);
    };
    Ok(HydrationRequest::new(
        request.mount_id,
        request.remote_id,
        virtual_fs_content_root(state_root, &mount.mount_id).join(entity.path),
        request.target_state,
        request.reason,
    ))
}

fn hydration_output_root_for_projection<S>(
    store: &S,
    state_root: &Path,
    request: &HydrationRequest,
) -> afs_core::AfsResult<Option<PathBuf>>
where
    S: MountRepository,
{
    let Some(mount) = store.get_mount(&request.mount_id).map_err(AfsError::from)? else {
        return Ok(None);
    };
    Ok(mount
        .projection
        .uses_virtual_filesystem()
        .then(|| virtual_fs_content_root(state_root, &mount.mount_id)))
}

fn run_job(
    runner: Arc<dyn RuntimeJobRunner>,
    state_root: PathBuf,
    policy: HydrationPolicy,
    job: MutatingJob,
) -> JobCompletion {
    match job {
        MutatingJob::Request(MutatingRequest::Pull { path, respond_to }) => JobCompletion::Pull {
            response: runner.run_pull(state_root, path),
            respond_to,
        },
        MutatingJob::Request(MutatingRequest::Push { job, respond_to }) => JobCompletion::Push {
            response: runner.run_push(state_root, job),
            respond_to,
        },
        MutatingJob::Request(MutatingRequest::FileEvent { event }) => {
            JobCompletion::FileEvent(runner.run_file_event(state_root, event))
        }
        MutatingJob::Request(MutatingRequest::VirtualFsRefreshChildren {
            mount_id,
            container_identifier,
        }) => {
            let result = runner.run_virtual_fs_refresh_children(
                state_root,
                mount_id.clone(),
                container_identifier.clone(),
            );
            JobCompletion::VirtualFsRefreshChildren {
                mount_id,
                container_identifier,
                result,
            }
        }
        MutatingJob::Request(MutatingRequest::VirtualFsMaterialize {
            mount_id,
            identifier,
            respond_to,
        }) => JobCompletion::Response {
            response: runner.run_virtual_fs_materialize(state_root, mount_id, identifier),
            respond_to,
        },
        MutatingJob::Request(MutatingRequest::FileProviderRead {
            mount_id,
            identifier,
            respond_to,
        }) => JobCompletion::Response {
            response: runner.run_file_provider_read(state_root, mount_id, identifier),
            respond_to,
        },
        MutatingJob::Request(MutatingRequest::VirtualFsCommitWrite {
            mount_id,
            identifier,
            contents_base64,
            respond_to,
        }) => JobCompletion::Response {
            response: runner.run_virtual_fs_commit_write(
                state_root,
                mount_id,
                identifier,
                contents_base64,
            ),
            respond_to,
        },
        MutatingJob::Request(MutatingRequest::VirtualFsCreateFile {
            mount_id,
            parent_identifier,
            filename,
            respond_to,
        }) => JobCompletion::Response {
            response: runner.run_virtual_fs_create_file(
                state_root,
                mount_id,
                parent_identifier,
                filename,
            ),
            respond_to,
        },
        MutatingJob::Request(MutatingRequest::VirtualFsRename {
            mount_id,
            identifier,
            new_parent_identifier,
            new_filename,
            respond_to,
        }) => JobCompletion::Response {
            response: runner.run_virtual_fs_rename(
                state_root,
                mount_id,
                identifier,
                new_parent_identifier,
                new_filename,
            ),
            respond_to,
        },
        MutatingJob::Request(MutatingRequest::VirtualFsTrash {
            mount_id,
            identifier,
            respond_to,
        }) => JobCompletion::Response {
            response: runner.run_virtual_fs_trash(state_root, mount_id, identifier),
            respond_to,
        },
        MutatingJob::ScheduledPull { tick } => {
            JobCompletion::ScheduledPull(runner.run_scheduled_pull(state_root, tick, policy))
        }
        MutatingJob::Hydration { request } => {
            let result = runner.run_hydration(state_root, request.clone());
            JobCompletion::Hydration { request, result }
        }
    }
}

enum RuntimeMessage {
    Request {
        request: DaemonRequest,
        respond_to: Sender<DaemonResponse>,
    },
    FileEvent(FileEvent),
    Status {
        respond_to: Sender<DaemonRuntimeStatus>,
    },
    JobFinished(JobCompletion),
    Shutdown,
}

enum MutatingRequest {
    Pull {
        path: PathBuf,
        respond_to: Sender<DaemonResponse>,
    },
    Push {
        job: PushJob,
        respond_to: Sender<DaemonResponse>,
    },
    FileEvent {
        event: FileEvent,
    },
    VirtualFsRefreshChildren {
        mount_id: String,
        container_identifier: String,
    },
    VirtualFsMaterialize {
        mount_id: String,
        identifier: String,
        respond_to: Sender<DaemonResponse>,
    },
    FileProviderRead {
        mount_id: String,
        identifier: String,
        respond_to: Sender<DaemonResponse>,
    },
    VirtualFsCommitWrite {
        mount_id: String,
        identifier: String,
        contents_base64: String,
        respond_to: Sender<DaemonResponse>,
    },
    VirtualFsCreateFile {
        mount_id: String,
        parent_identifier: String,
        filename: String,
        respond_to: Sender<DaemonResponse>,
    },
    VirtualFsRename {
        mount_id: String,
        identifier: String,
        new_parent_identifier: String,
        new_filename: String,
        respond_to: Sender<DaemonResponse>,
    },
    VirtualFsTrash {
        mount_id: String,
        identifier: String,
        respond_to: Sender<DaemonResponse>,
    },
}

enum MutatingJob {
    Request(MutatingRequest),
    ScheduledPull { tick: PullSchedulerTick },
    Hydration { request: HydrationRequest },
}

impl MutatingJob {
    fn active_status_parts(&self) -> (String, Option<String>) {
        match self {
            Self::Request(request) => request.active_status_parts(),
            Self::ScheduledPull { .. } => ("scheduled_pull".to_string(), None),
            Self::Hydration { request } => (
                "hydration".to_string(),
                Some(request.path.display().to_string()),
            ),
        }
    }
}

impl MutatingRequest {
    fn active_status_parts(&self) -> (String, Option<String>) {
        match self {
            Self::Pull { path, .. } => ("pull".to_string(), Some(path.display().to_string())),
            Self::Push { job, .. } => (
                "push".to_string(),
                Some(job.target_path.display().to_string()),
            ),
            Self::FileEvent { event } => (
                "file_event".to_string(),
                Some(event.path.display().to_string()),
            ),
            Self::VirtualFsRefreshChildren {
                mount_id,
                container_identifier,
            } => (
                "virtual_fs_refresh_children".to_string(),
                Some(format!("{mount_id}:{container_identifier}")),
            ),
            Self::VirtualFsMaterialize {
                mount_id,
                identifier,
                ..
            } => (
                "virtual_fs_materialize".to_string(),
                Some(format!("{mount_id}:{identifier}")),
            ),
            Self::FileProviderRead {
                mount_id,
                identifier,
                ..
            } => (
                "file_provider_read".to_string(),
                Some(format!("{mount_id}:{identifier}")),
            ),
            Self::VirtualFsCommitWrite {
                mount_id,
                identifier,
                ..
            } => (
                "virtual_fs_commit_write".to_string(),
                Some(format!("{mount_id}:{identifier}")),
            ),
            Self::VirtualFsCreateFile {
                mount_id,
                parent_identifier,
                filename,
                ..
            } => (
                "virtual_fs_create_file".to_string(),
                Some(format!("{mount_id}:{parent_identifier}/{filename}")),
            ),
            Self::VirtualFsRename {
                mount_id,
                identifier,
                new_filename,
                ..
            } => (
                "virtual_fs_rename".to_string(),
                Some(format!("{mount_id}:{identifier}->{new_filename}")),
            ),
            Self::VirtualFsTrash {
                mount_id,
                identifier,
                ..
            } => (
                "virtual_fs_trash".to_string(),
                Some(format!("{mount_id}:{identifier}")),
            ),
        }
    }
}

enum JobCompletion {
    Pull {
        response: DaemonResponse,
        respond_to: Sender<DaemonResponse>,
    },
    Push {
        response: DaemonResponse,
        respond_to: Sender<DaemonResponse>,
    },
    Response {
        response: DaemonResponse,
        respond_to: Sender<DaemonResponse>,
    },
    VirtualFsRefreshChildren {
        mount_id: String,
        container_identifier: String,
        result: afs_core::AfsResult<usize>,
    },
    ScheduledPull(afs_core::AfsResult<ScheduledPullRuntimeReport>),
    Hydration {
        request: HydrationRequest,
        result: afs_core::AfsResult<HydrationOutcome>,
    },
    FileEvent(afs_core::AfsResult<FileEventRuntimeReport>),
}

fn execute_file_event<S>(
    store: &mut S,
    event: FileEvent,
) -> afs_core::AfsResult<FileEventRuntimeReport>
where
    S: MountRepository + EntityRepository + ShadowRepository,
{
    let mut runtime_report = FileEventRuntimeReport::default();
    let Some((mount, entity)) = resolve_event_entity(store, &event.path)? else {
        runtime_report.report.ignored_events = 1;
        return Ok(runtime_report);
    };

    match event.kind {
        FileEventKind::Read => {
            handle_read_event(mount, entity, &mut runtime_report);
        }
        FileEventKind::Write => {
            handle_write_event(store, mount, entity, event.path, &mut runtime_report.report)?;
        }
        FileEventKind::Rename | FileEventKind::Remove => runtime_report.report.ignored_events = 1,
    }

    Ok(runtime_report)
}

fn handle_read_event(
    mount: MountConfig,
    entity: EntityRecord,
    runtime_report: &mut FileEventRuntimeReport,
) {
    if !should_hydrate_on_read(&entity) {
        runtime_report.report.ignored_events = 1;
        return;
    }

    runtime_report.queued_hydrations.push(HydrationRequest::new(
        mount.mount_id.clone(),
        entity.remote_id,
        mount.root.join(&entity.path),
        HydrationState::Hydrated,
        HydrationReason::StubRead,
    ));
    runtime_report.report.queued_hydrations = 1;
}

fn handle_write_event<S>(
    store: &mut S,
    mount: MountConfig,
    mut entity: EntityRecord,
    event_path: PathBuf,
    report: &mut DaemonEventReport,
) -> afs_core::AfsResult<()>
where
    S: EntityRepository + ShadowRepository,
{
    if entity.hydration != HydrationState::Hydrated {
        report.ignored_events = 1;
        return Ok(());
    }

    if hydrated_file_matches_shadow(store, &mount, &entity, &event_path)? {
        report.ignored_events = 1;
        return Ok(());
    }

    entity.hydration = HydrationState::Dirty;
    store.save_entity(entity).map_err(AfsError::from)?;
    report.marked_dirty = 1;
    Ok(())
}

fn hydrated_file_matches_shadow<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    event_path: &std::path::Path,
) -> afs_core::AfsResult<bool>
where
    S: ShadowRepository,
{
    let contents = match std::fs::read_to_string(event_path) {
        Ok(contents) => contents,
        Err(_) => return Ok(false),
    };
    let parsed = match parse_canonical_markdown(&contents) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(false),
    };
    let shadow = match store.load_shadow(&mount.mount_id, &entity.remote_id) {
        Ok(shadow) => shadow,
        Err(_) => return Ok(false),
    };

    Ok(parsed.document.frontmatter == shadow.frontmatter
        && parsed.document.body == shadow.rendered_body)
}

fn should_hydrate_on_read(entity: &EntityRecord) -> bool {
    if entity.kind != EntityKind::Page {
        return false;
    }

    matches!(
        entity.hydration,
        HydrationState::Virtual | HydrationState::Stub
    )
}

fn resolve_event_entity<S>(
    store: &S,
    event_path: &std::path::Path,
) -> afs_core::AfsResult<Option<(MountConfig, EntityRecord)>>
where
    S: MountRepository + EntityRepository,
{
    let mounts = store.load_mounts().map_err(AfsError::from)?;
    for mount in &mounts {
        let Some(relative_path) = event_relative_path(&mount.root, event_path) else {
            continue;
        };
        if relative_path.as_os_str().is_empty() {
            continue;
        }

        if let Some(entity) = store
            .find_entity_by_path(&mount.mount_id, &relative_path)
            .map_err(AfsError::from)?
        {
            return Ok(Some((mount.clone(), entity)));
        }
    }

    if mounts.len() == 1
        && event_path.is_relative()
        && let Some(mount) = mounts.first()
        && let Some(entity) = store
            .find_entity_by_path(&mount.mount_id, event_path)
            .map_err(AfsError::from)?
    {
        return Ok(Some((mount.clone(), entity)));
    }

    Ok(None)
}

fn event_relative_path(root: &std::path::Path, event_path: &std::path::Path) -> Option<PathBuf> {
    if let Ok(relative) = event_path.strip_prefix(root) {
        return Some(relative.to_path_buf());
    }

    let canonical_root = std::fs::canonicalize(root).ok()?;
    let canonical_event_path = std::fs::canonicalize(event_path).ok()?;
    canonical_event_path
        .strip_prefix(canonical_root)
        .ok()
        .map(PathBuf::from)
}

fn afs_error_code(error: &AfsError) -> &'static str {
    match error {
        AfsError::Validation(_) => "validation_failed",
        AfsError::Conflict(_) => "conflict",
        AfsError::Guardrail(_) => "guardrail",
        AfsError::InvalidState(_) => "invalid_state",
        AfsError::Unsupported(_) => "unsupported",
        AfsError::NotImplemented(_) => "not_implemented",
        AfsError::Io(_) => "io_error",
    }
}
