//! Daemon runtime control loop.
//!
//! Socket handlers submit requests here instead of executing sync code directly.
//! The runtime keeps mutating work serialized while slow connector calls run on
//! worker threads, so health checks and future control-plane work stay
//! responsive during network I/O.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
#[cfg(target_os = "macos")]
use std::env;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use locality_connector::{Connector, ObserveRequest};
use locality_core::LocalityError;
use locality_core::canonical::parse_canonical_markdown;
use locality_core::diff::{BlockDiffEngine, DiffEngine};
use locality_core::freshness::{
    ChangeHintKind, FreshnessOptimizationPolicy, FreshnessTier, RemoteObservation, SyncJob,
    SyncJobKind,
};
use locality_core::hydration::{HydrationPolicy, HydrationReason, HydrationRequest};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId};
use locality_core::planner::PushOperation;
use locality_core::pull::PullMode;
use locality_core::shadow::{ShadowDocument, rendered_bodies_equivalent};
use locality_notion::client::{notion_request_debug_status, notion_requests_per_second_setting};
use locality_store::{
    AutoSaveRepository, AutoSaveState, EntityRecord, EntityRepository, FreshnessStateRecord,
    FreshnessStateRepository, HydrationJobRecord, HydrationJobRepository,
    MetadataDiscoveryJobRecord, MetadataDiscoveryJobRepository, MetadataDiscoveryPriority,
    MountConfig, MountLiveModeRepository, MountRepository, ProjectionMode, RemoteObservationRecord,
    RemoteObservationRepository, ShadowRepository, SqliteStateStore, open_credential_store,
};
use serde_json::{Value, json};

use crate::DaemonConfig;
use crate::autosave::{auto_save_target_for_write, pause_auto_save_for_remote_change};
use crate::execution::{DaemonEventReport, PushJob};
use crate::file_provider::{self, FileProviderReadReport};
use crate::freshness::{
    FreshnessQueue, freshness_timestamp, freshness_unix_ms, optimized_freshness_decision,
    parse_freshness_timestamp, record_file_opened, record_local_change,
};
use crate::hydration::{
    HydrationEngine, HydrationExecutor, HydrationOutcome, HydrationPriority, HydrationQueue,
    hydration_priority,
};
use crate::ipc::{
    DaemonActiveJobStatus, DaemonDebugQueueItem, DaemonDebugQueueSection, DaemonDebugQueueStatus,
    DaemonRequest, DaemonResponse, DaemonRuntimeStatus,
};
use crate::pull::run_pull_with_state_root;
use crate::push::{
    execute_auto_save_push_job_with_content_root, execute_push_job_with_content_root,
};
use crate::reconcile::{
    DefaultFetchScheduleStrategy, ScheduledPullReport, reconcile_scheduled_pull_with_state_root,
};
use crate::scheduler::{PullScheduler, PullSchedulerTick};
use crate::shadow_match::parsed_matches_shadow;
use crate::source::{ResolvedSourceSet, resolve_source_for_mount_id, resolve_source_for_path};
use crate::virtual_fs::{
    MOUNT_POINT_PREFIX, ROOT_CONTAINER_IDENTIFIER, VirtualFsRefreshChildrenReport,
    commit_virtual_fs_write, create_virtual_fs_directory, create_virtual_fs_file,
    materialize_virtual_fs_guidance_with_content_root,
    materialize_virtual_fs_item_with_content_root, mount_point_identifier,
    refresh_virtual_fs_children, rename_virtual_fs_item, trash_virtual_fs_item,
    virtual_fs_children_refresh_needed, virtual_fs_children_with_content_root,
    virtual_fs_content_root, virtual_fs_item_with_content_root,
};
use crate::watcher::{FileEvent, FileEventKind};

#[derive(Clone)]
pub struct DaemonRuntimeHandle {
    sender: Sender<RuntimeMessage>,
}

const CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const FRESHNESS_JOB_BUDGET_UNITS: u16 = 5;
const MAX_WORKSPACE_FRESHNESS_JOBS_PER_TICK: usize = 100;
const LIVE_MODE_REMOTE_OBSERVE_QUEUE_SHARE: f64 = 1.0 / 3.0;
const LIVE_MODE_REMOTE_OBSERVE_MAX_QUEUE_JOBS: usize = FRESHNESS_JOB_BUDGET_UNITS as usize;
const AUTO_FAST_FORWARD_ACTIVE_LEASE_MS: u64 = 30_000;
const MAX_CHILD_REFRESH_WORKERS: usize = 3;
const MAX_BACKGROUND_CHILD_REFRESH_WORKERS: usize = 2;
const DEBUG_QUEUE_ITEM_LIMIT: usize = 25;

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

    pub fn prime_virtual_mounts(&self) -> Result<(), RuntimeSendError> {
        self.sender
            .send(RuntimeMessage::PrimeVirtualMounts)
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
    pub fn spawn(config: DaemonConfig) -> locality_core::LocalityResult<Self> {
        Self::spawn_with_runner(config, DefaultRuntimeJobRunner)
    }

    pub fn spawn_with_runner<Runner>(
        config: DaemonConfig,
        runner: Runner,
    ) -> locality_core::LocalityResult<Self>
    where
        Runner: RuntimeJobRunner,
    {
        std::fs::create_dir_all(&config.state_root)?;
        SqliteStateStore::open(config.state_root.clone()).map_err(LocalityError::from)?;
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

    fn run_auto_push(&self, state_root: PathBuf, job: PushJob) -> DaemonResponse {
        self.run_push(state_root, job)
    }

    fn run_scheduled_pull(
        &self,
        state_root: PathBuf,
        tick: PullSchedulerTick,
        policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport>;

    fn run_hydration(
        &self,
        state_root: PathBuf,
        request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome>;

    fn run_file_event(
        &self,
        _state_root: PathBuf,
        _event: FileEvent,
    ) -> locality_core::LocalityResult<FileEventRuntimeReport> {
        Err(LocalityError::Unsupported(
            "runtime runner does not handle file events",
        ))
    }

    fn run_freshness_job(
        &self,
        _state_root: PathBuf,
        _job: SyncJob,
    ) -> locality_core::LocalityResult<FreshnessRuntimeReport> {
        Err(LocalityError::Unsupported(
            "runtime runner does not handle freshness jobs",
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

    fn run_virtual_projection_root_children(
        &self,
        _state_root: PathBuf,
        _projection_root: PathBuf,
        _projection: ProjectionMode,
    ) -> DaemonResponse {
        DaemonResponse::error(
            "unsupported",
            "runtime runner does not handle shared projection root child enumeration",
        )
    }

    fn run_virtual_fs_refresh_children(
        &self,
        _state_root: PathBuf,
        _mount_id: String,
        _container_identifier: String,
    ) -> locality_core::LocalityResult<VirtualFsRefreshChildrenReport> {
        Err(LocalityError::Unsupported(
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

    fn run_virtual_fs_create_directory(
        &self,
        _state_root: PathBuf,
        _mount_id: String,
        _parent_identifier: String,
        _dirname: String,
    ) -> DaemonResponse {
        DaemonResponse::error(
            "unsupported",
            "runtime runner does not handle virtual filesystem directory creates",
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

    fn run_file_provider_domain_children(
        &self,
        _state_root: PathBuf,
        _domain_id: String,
    ) -> DaemonResponse {
        DaemonResponse::error(
            "unsupported",
            "runtime runner does not handle File Provider domain enumeration",
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileEventRuntimeReport {
    pub report: DaemonEventReport,
    pub queued_hydrations: Vec<HydrationRequest>,
    pub freshness_jobs: Vec<SyncJob>,
    pub auto_push_targets: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledPullRuntimeReport {
    pub report: ScheduledPullReport,
    pub queued_hydrations: Vec<HydrationRequest>,
    pub freshness_jobs: Vec<SyncJob>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreshnessRuntimeReport {
    pub job: SyncJob,
    pub remote_hint_pending: bool,
    pub queued_hydrations: Vec<HydrationRequest>,
    pub follow_up_jobs: Vec<SyncJob>,
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
            Err(error) => DaemonResponse::error(locality_error_code(&error), error.to_string()),
        }
    }

    fn run_auto_push(&self, state_root: PathBuf, job: PushJob) -> DaemonResponse {
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

        match execute_auto_save_push_job_with_content_root(
            &mut store,
            job,
            &connector,
            Some(&state_root),
        ) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(locality_error_code(&error), error.to_string()),
        }
    }

    fn run_scheduled_pull(
        &self,
        state_root: PathBuf,
        tick: PullSchedulerTick,
        policy: HydrationPolicy,
    ) -> locality_core::LocalityResult<ScheduledPullRuntimeReport> {
        let mut store = SqliteStateStore::open(state_root.clone()).map_err(LocalityError::from)?;
        let mounts = store.load_mounts().map_err(LocalityError::from)?;
        let credentials = open_credential_store(&state_root);
        let source = ResolvedSourceSet::new(&store, credentials.as_ref(), &mounts)
            .map_err(LocalityError::from)?;
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
        let freshness_jobs = workspace_virtual_freshness_jobs(&store, &mounts, &tick)?;

        Ok(ScheduledPullRuntimeReport {
            report,
            queued_hydrations: hydration.into_requests(),
            freshness_jobs,
        })
    }

    fn run_hydration(
        &self,
        state_root: PathBuf,
        request: HydrationRequest,
    ) -> locality_core::LocalityResult<HydrationOutcome> {
        let mut store = SqliteStateStore::open(state_root.clone()).map_err(LocalityError::from)?;
        let request = hydration_request_for_projection(&store, &state_root, request)?;
        let credentials = open_credential_store(&state_root);
        let connector =
            resolve_source_for_mount_id(&store, credentials.as_ref(), &request.mount_id)
                .map_err(LocalityError::from)?;
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
    ) -> locality_core::LocalityResult<FileEventRuntimeReport> {
        let mut store = SqliteStateStore::open(state_root).map_err(LocalityError::from)?;
        execute_file_event(&mut store, event)
    }

    fn run_freshness_job(
        &self,
        state_root: PathBuf,
        job: SyncJob,
    ) -> locality_core::LocalityResult<FreshnessRuntimeReport> {
        let mut store = SqliteStateStore::open(state_root.clone()).map_err(LocalityError::from)?;
        let credentials = open_credential_store(&state_root);
        let connector = resolve_source_for_mount_id(&store, credentials.as_ref(), &job.mount_id)
            .map_err(LocalityError::from)?;

        execute_freshness_job(&mut store, &connector, job)
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
            Err(error) => DaemonResponse::error(locality_error_code(&error), error.to_string()),
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
            Err(error) => DaemonResponse::error(locality_error_code(&error), error.to_string()),
        }
    }

    fn run_virtual_projection_root_children(
        &self,
        state_root: PathBuf,
        projection_root: PathBuf,
        projection: ProjectionMode,
    ) -> DaemonResponse {
        let store = match SqliteStateStore::open(state_root) {
            Ok(store) => store,
            Err(error) => return DaemonResponse::error("store_open_failed", error.to_string()),
        };
        match crate::virtual_projection::virtual_projection_root_children(
            &store,
            &projection_root,
            projection,
        ) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(locality_error_code(&error), error.to_string()),
        }
    }

    fn run_virtual_fs_refresh_children(
        &self,
        state_root: PathBuf,
        mount_id: String,
        container_identifier: String,
    ) -> locality_core::LocalityResult<VirtualFsRefreshChildrenReport> {
        let mut store = SqliteStateStore::open(state_root.clone()).map_err(LocalityError::from)?;
        let mount_id = MountId::new(mount_id);
        ensure_virtual_fs_mount(&store, &mount_id)?;
        if !virtual_fs_children_refresh_needed(&store, &mount_id, &container_identifier)? {
            return Ok(VirtualFsRefreshChildrenReport::default());
        }
        let credentials = open_credential_store(&state_root);
        let connector = resolve_source_for_mount_id(&store, credentials.as_ref(), &mount_id)
            .map_err(LocalityError::from)?;
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
        let content_root = virtual_fs_content_root(&state_root, &mount_id);
        match materialize_virtual_fs_guidance_with_content_root(
            &store,
            &content_root,
            &mount_id,
            &identifier,
        ) {
            Ok(Some(report)) => return DaemonResponse::ok(report),
            Ok(None) => {}
            Err(error) => {
                return DaemonResponse::error(locality_error_code(&error), error.to_string());
            }
        }
        let credentials = open_credential_store(&state_root);
        let connector = match resolve_source_for_mount_id(&store, credentials.as_ref(), &mount_id) {
            Ok(connector) => connector,
            Err(error) => return DaemonResponse::error(error.code(), error.message()),
        };
        match materialize_virtual_fs_item_with_content_root(
            &mut store,
            &connector,
            &content_root,
            &mount_id,
            &identifier,
        ) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(locality_error_code(&error), error.to_string()),
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
        let content_root = virtual_fs_content_root(&state_root, &mount_id);
        match materialize_virtual_fs_guidance_with_content_root(
            &store,
            &content_root,
            &mount_id,
            &identifier,
        ) {
            Ok(Some(materialized)) => {
                return file_provider_read_materialized(
                    &store,
                    &content_root,
                    &mount_id,
                    &identifier,
                    materialized,
                );
            }
            Ok(None) => {}
            Err(error) => {
                return DaemonResponse::error(locality_error_code(&error), error.to_string());
            }
        }
        let credentials = open_credential_store(&state_root);
        let connector = match resolve_source_for_mount_id(&store, credentials.as_ref(), &mount_id) {
            Ok(connector) => connector,
            Err(error) => return DaemonResponse::error(error.code(), error.message()),
        };
        let materialized = match materialize_virtual_fs_item_with_content_root(
            &mut store,
            &connector,
            &content_root,
            &mount_id,
            &identifier,
        ) {
            Ok(report) => report,
            Err(error) => {
                return DaemonResponse::error(locality_error_code(&error), error.to_string());
            }
        };
        file_provider_read_materialized(&store, &content_root, &mount_id, &identifier, materialized)
    }

    fn run_file_provider_domain_children(
        &self,
        state_root: PathBuf,
        domain_id: String,
    ) -> DaemonResponse {
        let store = match SqliteStateStore::open(state_root) {
            Ok(store) => store,
            Err(error) => return DaemonResponse::error("store_open_failed", error.to_string()),
        };
        match file_provider::file_provider_domain_children(&store, &domain_id) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(locality_error_code(&error), error.to_string()),
        }
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
            Err(error) => DaemonResponse::error(locality_error_code(&error), error.to_string()),
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
            Err(error) => DaemonResponse::error(locality_error_code(&error), error.to_string()),
        }
    }

    fn run_virtual_fs_create_directory(
        &self,
        state_root: PathBuf,
        mount_id: String,
        parent_identifier: String,
        dirname: String,
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
        match create_virtual_fs_directory(
            &mut store,
            &content_root,
            &mount_id,
            &parent_identifier,
            &dirname,
        ) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error(locality_error_code(&error), error.to_string()),
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
            Err(error) => DaemonResponse::error(locality_error_code(&error), error.to_string()),
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
            Err(error) => DaemonResponse::error(locality_error_code(&error), error.to_string()),
        }
    }
}

fn file_provider_read_materialized(
    store: &SqliteStateStore,
    content_root: &Path,
    mount_id: &MountId,
    identifier: &str,
    materialized: crate::virtual_fs::VirtualFsMaterializeReport,
) -> DaemonResponse {
    let contents = match std::fs::read(&materialized.path) {
        Ok(contents) => contents,
        Err(error) => return DaemonResponse::error("read_failed", error.to_string()),
    };
    let item = match virtual_fs_item_with_content_root(store, content_root, mount_id, identifier) {
        Ok(report) => report.item,
        Err(error) => {
            return DaemonResponse::error(locality_error_code(&error), error.to_string());
        }
    };

    DaemonResponse::ok(FileProviderReadReport {
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

fn reject_plain_files_virtual_fs_mount<S>(store: &S, mount_id: &MountId) -> Option<DaemonResponse>
where
    S: MountRepository,
{
    match store.get_mount(mount_id) {
        Ok(Some(mount)) if !mount.projection.uses_virtual_filesystem() => {
            let error = LocalityError::Unsupported(
                "plain-files mounts do not support virtual filesystem operations",
            );
            Some(DaemonResponse::error(
                locality_error_code(&error),
                error.to_string(),
            ))
        }
        Ok(_) => None,
        Err(error) => Some(DaemonResponse::error("store_error", error.to_string())),
    }
}

fn ensure_virtual_fs_mount<S>(store: &S, mount_id: &MountId) -> locality_core::LocalityResult<()>
where
    S: MountRepository,
{
    match store.get_mount(mount_id).map_err(LocalityError::from)? {
        Some(mount) if !mount.projection.uses_virtual_filesystem() => {
            Err(LocalityError::Unsupported(
                "plain-files mounts do not support virtual filesystem operations",
            ))
        }
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
    fn queue(&mut self, request: HydrationRequest) -> locality_core::LocalityResult<()> {
        self.requests.push(request);
        Ok(())
    }

    fn drain_ready(&mut self) -> locality_core::LocalityResult<usize> {
        let count = self.requests.len();
        self.requests.clear();
        Ok(count)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ChildRefreshPriority {
    Background,
    Interactive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ChildRefreshRequest {
    mount_id: String,
    container_identifier: String,
    priority: ChildRefreshPriority,
    depth: u32,
}

impl ChildRefreshRequest {
    fn key(&self) -> ChildRefreshKey {
        ChildRefreshKey {
            mount_id: self.mount_id.clone(),
            container_identifier: self.container_identifier.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ChildRefreshKey {
    mount_id: String,
    container_identifier: String,
}

#[derive(Default)]
struct ChildRefreshQueue {
    order: VecDeque<ChildRefreshKey>,
    pending: BTreeMap<ChildRefreshKey, ChildRefreshRequest>,
}

impl ChildRefreshQueue {
    fn len(&self) -> usize {
        self.pending.len()
    }

    fn queue(&mut self, request: ChildRefreshRequest) -> bool {
        let key = request.key();
        let inserted = !self.pending.contains_key(&key);
        if inserted {
            self.order.push_back(key.clone());
            self.pending.insert(key, request);
            return true;
        }

        if let Some(existing) = self.pending.get_mut(&key) {
            if request.priority > existing.priority {
                existing.priority = request.priority;
            }
            existing.depth = existing.depth.min(request.depth);
        }
        false
    }

    fn pop_ready(
        &mut self,
        active: &BTreeMap<ChildRefreshKey, ActiveChildRefresh>,
    ) -> Option<ChildRefreshRequest> {
        let index = self.next_ready_index(active)?;
        let key = self.order.remove(index)?;
        self.pending.remove(&key)
    }

    fn debug_requests(&self, limit: usize) -> Vec<ChildRefreshRequest> {
        let mut requests = self
            .order
            .iter()
            .enumerate()
            .filter_map(|(index, key)| {
                self.pending
                    .get(key)
                    .cloned()
                    .map(|request| (index, request))
            })
            .collect::<Vec<_>>();
        requests.sort_by(|(left_index, left), (right_index, right)| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.depth.cmp(&right.depth))
                .then_with(|| left_index.cmp(right_index))
        });
        requests
            .into_iter()
            .take(limit)
            .map(|(_, request)| request)
            .collect()
    }

    fn next_ready_index(
        &self,
        active: &BTreeMap<ChildRefreshKey, ActiveChildRefresh>,
    ) -> Option<usize> {
        let mut best: Option<(usize, ChildRefreshPriority, u32)> = None;
        for (index, key) in self.order.iter().enumerate() {
            let Some(request) = self.pending.get(key) else {
                continue;
            };
            if active.values().any(|active| {
                active.request.priority == request.priority && active.request.depth < request.depth
            }) {
                continue;
            }
            if best.as_ref().is_none_or(|(_, best_priority, best_depth)| {
                request.priority > *best_priority
                    || (request.priority == *best_priority && request.depth < *best_depth)
            }) {
                best = Some((index, request.priority, request.depth));
            }
        }
        best.map(|(index, _, _)| index)
    }
}

struct RuntimeState {
    config: DaemonConfig,
    runner: Arc<dyn RuntimeJobRunner>,
    sender: Sender<RuntimeMessage>,
    pending_requests: VecDeque<MutatingRequest>,
    child_refreshes: ChildRefreshQueue,
    active_child_refreshes: BTreeMap<ChildRefreshKey, ActiveChildRefresh>,
    completed_child_refreshes: BTreeMap<ChildRefreshKey, ChildRefreshPriority>,
    hydration: HydrationQueue,
    freshness: FreshnessQueue,
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

#[derive(Clone, Debug)]
struct ActiveChildRefresh {
    request: ChildRefreshRequest,
    status: ActiveRuntimeJob,
}

impl ActiveChildRefresh {
    fn new(request: ChildRefreshRequest) -> Self {
        let status = ActiveRuntimeJob::from_child_refresh(&request);
        Self { request, status }
    }
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

    fn from_child_refresh(request: &ChildRefreshRequest) -> Self {
        Self {
            kind: "virtual_fs_refresh_children".to_string(),
            target: Some(format!(
                "{}:{}",
                request.mount_id, request.container_identifier
            )),
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
        let child_refreshes = if config.background_connector_sync {
            load_persisted_child_refreshes(&config.state_root)
        } else {
            ChildRefreshQueue::default()
        };

        Self {
            scheduler: PullScheduler::new(config.pull_scheduler.clone()),
            last_scheduler_advance: Instant::now(),
            config,
            runner,
            sender,
            pending_requests: VecDeque::new(),
            child_refreshes,
            active_child_refreshes: BTreeMap::new(),
            completed_child_refreshes: BTreeMap::new(),
            hydration,
            freshness: FreshnessQueue::new(),
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
                }) => {
                    if matches!(
                        self.handle_request(request, respond_to),
                        RuntimeLoopDecision::Stop
                    ) {
                        break;
                    }
                }
                Ok(RuntimeMessage::FileEvent(event)) => self.handle_file_event(event),
                Ok(RuntimeMessage::Status { respond_to }) => {
                    let _ = respond_to.send(self.status());
                }
                Ok(RuntimeMessage::PrimeVirtualMounts) => self.prime_virtual_mounts(),
                Ok(RuntimeMessage::JobFinished(completion)) => self.handle_completion(completion),
                Ok(RuntimeMessage::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => self.handle_timeout(),
            }
        }
    }

    fn handle_request(
        &mut self,
        request: DaemonRequest,
        respond_to: Sender<DaemonResponse>,
    ) -> RuntimeLoopDecision {
        match request {
            DaemonRequest::Ping => {
                let _ = respond_to.send(DaemonResponse::ok(json!({ "status": "ok" })));
            }
            DaemonRequest::Status => {
                let _ = respond_to.send(DaemonResponse::ok(self.status()));
            }
            DaemonRequest::DebugQueueStatus => {
                let _ = respond_to.send(DaemonResponse::ok(self.debug_queue_status()));
            }
            DaemonRequest::ReloadMounts => {
                let _ = respond_to.send(DaemonResponse::error(
                    "unsupported",
                    "mount reload is handled by the daemon server",
                ));
            }
            DaemonRequest::Shutdown => {
                let _ = respond_to.send(DaemonResponse::ok(json!({
                    "status": "shutting_down"
                })));
                return RuntimeLoopDecision::Stop;
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
            DaemonRequest::Hydrate {
                mount_id,
                remote_id,
                path,
            } => {
                self.queue_hydration(HydrationRequest::new(
                    MountId::new(mount_id.clone()),
                    RemoteId::new(remote_id.clone()),
                    path.clone(),
                    HydrationState::Hydrated,
                    HydrationReason::FileOpen,
                ));
                let _ = respond_to.send(DaemonResponse::ok(json!({
                    "queued": true,
                    "mount_id": mount_id,
                    "remote_id": remote_id,
                    "path": path,
                })));
                self.maybe_start_next_job();
            }
            DaemonRequest::ObserveEntity {
                mount_id,
                remote_id,
            } => {
                let queued = self.queue_bounded_live_mode_remote_observe(
                    SyncJob::new(
                        MountId::new(mount_id.clone()),
                        Some(RemoteId::new(remote_id.clone())),
                        SyncJobKind::ObserveEntity,
                        ChangeHintKind::RemoteMaybeChanged,
                    )
                    .with_tier(FreshnessTier::Immediate),
                );
                let _ = respond_to.send(DaemonResponse::ok(json!({
                    "queued": queued,
                    "mount_id": mount_id,
                    "remote_id": remote_id,
                })));
                self.maybe_start_next_job();
            }
            DaemonRequest::RemoteFastForward {
                mount_id,
                remote_id,
                path,
            } => {
                self.queue_hydration(HydrationRequest::new(
                    MountId::new(mount_id.clone()),
                    RemoteId::new(remote_id.clone()),
                    path.clone(),
                    HydrationState::Hydrated,
                    HydrationReason::LiveModeRemoteFastForward,
                ));
                let _ = respond_to.send(DaemonResponse::ok(json!({
                    "queued": true,
                    "mount_id": mount_id,
                    "remote_id": remote_id,
                    "path": path,
                })));
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
            } => {
                let response = self.runner.run_virtual_fs_children(
                    self.config.state_root.clone(),
                    mount_id.clone(),
                    container_identifier.clone(),
                );
                let should_refresh = response.ok;
                let _ = respond_to.send(response);
                if should_refresh {
                    self.queue_child_refresh(
                        mount_id,
                        container_identifier,
                        ChildRefreshPriority::Interactive,
                        0,
                    );
                }
            }
            DaemonRequest::VirtualProjectionRootChildren {
                projection_root,
                projection,
            } => {
                let response = self.runner.run_virtual_projection_root_children(
                    self.config.state_root.clone(),
                    projection_root,
                    projection,
                );
                let _ = respond_to.send(response);
            }
            DaemonRequest::FileProviderChildren {
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
                    self.queue_child_refresh(
                        mount_id,
                        container_identifier,
                        ChildRefreshPriority::Interactive,
                        0,
                    );
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
            DaemonRequest::FileProviderDomainChildren { domain_id } => {
                let response = self
                    .runner
                    .run_file_provider_domain_children(self.config.state_root.clone(), domain_id);
                let _ = respond_to.send(response);
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
            DaemonRequest::VirtualFsCreateDirectory {
                mount_id,
                parent_identifier,
                dirname,
            } => {
                self.pending_requests
                    .push_front(MutatingRequest::VirtualFsCreateDirectory {
                        mount_id,
                        parent_identifier,
                        dirname,
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
        RuntimeLoopDecision::Continue
    }

    fn handle_file_event(&mut self, event: FileEvent) {
        eprintln!(
            "localityd local file event queued immediately for `{}`",
            event.path.display()
        );
        self.pending_requests
            .push_front(MutatingRequest::FileEvent { event });
        self.maybe_start_next_job();
    }

    fn handle_completion(&mut self, completion: JobCompletion) {
        match &completion {
            JobCompletion::VirtualFsRefreshChildren {
                mount_id,
                container_identifier,
                ..
            } => {
                let key = ChildRefreshKey {
                    mount_id: mount_id.clone(),
                    container_identifier: container_identifier.clone(),
                };
                self.active_child_refreshes.remove(&key);
            }
            _ => self.active_job = None,
        }

        match completion {
            JobCompletion::Pull {
                response,
                respond_to,
            }
            | JobCompletion::Push {
                response,
                respond_to,
            } => {
                let _ = respond_to.send(response);
            }
            JobCompletion::AutoPush {
                target_path,
                response,
            } => {
                if response.ok {
                    eprintln!(
                        "localityd auto-save push completed for `{}`",
                        target_path.display()
                    );
                } else {
                    let message = response
                        .error
                        .as_ref()
                        .map(|error| error.message.as_str())
                        .unwrap_or("unknown error");
                    eprintln!(
                        "localityd auto-save push failed for `{}`: {message}",
                        target_path.display()
                    );
                }
            }
            JobCompletion::Response {
                response,
                respond_to,
                freshness_jobs,
                auto_push_targets,
            } => {
                let _ = respond_to.send(response);
                for job in freshness_jobs {
                    self.queue_freshness(job);
                }
                for target in auto_push_targets {
                    self.queue_auto_push(target);
                }
            }
            JobCompletion::VirtualFsRefreshChildren {
                mount_id,
                container_identifier,
                depth,
                priority,
                result,
            } => match result {
                Ok(report) => {
                    self.delete_metadata_discovery_job(&mount_id, &container_identifier);
                    self.mark_child_refresh_completed(&mount_id, &container_identifier, priority);
                    if report.changed {
                        self.signal_macos_file_provider_container(&mount_id, &container_identifier);
                    }
                }
                Err(error) => {
                    self.record_metadata_discovery_failure(
                        &mount_id,
                        &container_identifier,
                        error.to_string(),
                    );
                    if self.config.background_connector_sync {
                        self.child_refreshes.queue(ChildRefreshRequest {
                            mount_id: mount_id.clone(),
                            container_identifier: container_identifier.clone(),
                            priority,
                            depth,
                        });
                    }
                    eprintln!(
                        "localityd virtual filesystem child refresh failed for `{mount_id}:{container_identifier}`: {error}"
                    );
                }
            },
            JobCompletion::ScheduledPull(result) => match result {
                Ok(result) => {
                    for request in result.queued_hydrations {
                        self.queue_hydration(request);
                    }
                    for job in result.freshness_jobs {
                        self.queue_freshness(job);
                    }
                }
                Err(error) => eprintln!("localityd scheduled pull failed: {error}"),
            },
            JobCompletion::Hydration {
                request,
                result,
                previous_shadow,
            } => match result {
                Ok(outcome) => {
                    self.delete_hydration_job(&request);
                    if outcome == HydrationOutcome::Hydrated {
                        self.refresh_visible_projection_after_remote_fast_forward(
                            &request,
                            previous_shadow.as_ref(),
                        );
                        self.queue_remote_fast_forward_discovery_hints(
                            &request,
                            previous_shadow.as_ref(),
                        );
                    }
                }
                Err(error) => {
                    eprintln!(
                        "localityd hydration failed for `{}`: {error}",
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
                    for job in result.freshness_jobs {
                        self.queue_freshness(job);
                    }
                    for target in result.auto_push_targets {
                        self.queue_auto_push(target);
                    }
                }
                Err(error) => eprintln!("localityd file event failed: {error}"),
            },
            JobCompletion::Freshness(result) => match result {
                Ok(result) => {
                    for request in result.queued_hydrations {
                        self.queue_hydration(request);
                    }
                    for job in result.follow_up_jobs {
                        self.queue_freshness(job);
                    }
                }
                Err(error) => eprintln!("localityd freshness job failed: {error}"),
            },
        }

        self.maybe_start_next_job();
    }

    fn signal_macos_file_provider_container(&self, mount_id: &str, container_identifier: &str) {
        let store = match SqliteStateStore::open(self.config.state_root.clone()) {
            Ok(store) => store,
            Err(error) => {
                eprintln!(
                    "localityd failed to open state for macOS File Provider invalidation: {error}"
                );
                return;
            }
        };
        let mount = match store.get_mount(&MountId::new(mount_id.to_string())) {
            Ok(Some(mount)) => mount,
            Ok(None) => return,
            Err(error) => {
                eprintln!(
                    "localityd failed to inspect mount for macOS File Provider invalidation: {error}"
                );
                return;
            }
        };
        if mount.projection != ProjectionMode::MacosFileProvider {
            return;
        }
        if let Err(error) = signal_macos_file_provider_enumerator(mount_id, container_identifier) {
            eprintln!(
                "localityd failed to signal macOS File Provider for `{mount_id}:{container_identifier}`: {error}"
            );
        }
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
            Err(error) => eprintln!("localityd scheduler tick failed: {error}"),
        }

        self.maybe_start_next_job();
    }

    fn maybe_start_next_job(&mut self) {
        if self.active_job.is_none() {
            let job = if let Some(request) = self.pending_requests.pop_front() {
                Some(MutatingJob::Request(request))
            } else if let Some(request) = self.hydration.pop_ready() {
                Some(MutatingJob::Hydration { request })
            } else if let Some(job) = self
                .freshness
                .pop_ready_at(Some(&freshness_timestamp()), FRESHNESS_JOB_BUDGET_UNITS)
            {
                Some(MutatingJob::Freshness { job })
            } else {
                self.pending_scheduled_tick
                    .take()
                    .map(|tick| MutatingJob::ScheduledPull { tick })
            };

            if let Some(job) = job {
                self.start_exclusive_job(job);
            }
        }

        self.maybe_start_child_refresh_jobs();
    }

    fn start_exclusive_job(&mut self, job: MutatingJob) {
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

    fn maybe_start_child_refresh_jobs(&mut self) {
        if !self.config.background_connector_sync {
            return;
        }
        while self.active_job.is_none()
            && self.active_child_refreshes.len() < MAX_CHILD_REFRESH_WORKERS
        {
            let Some(request) = self.child_refreshes.pop_ready(&self.active_child_refreshes) else {
                break;
            };

            if request.priority == ChildRefreshPriority::Background
                && self.active_background_child_refreshes() >= MAX_BACKGROUND_CHILD_REFRESH_WORKERS
            {
                self.child_refreshes.queue(request);
                break;
            }

            self.start_child_refresh_job(request);
        }
    }

    fn active_background_child_refreshes(&self) -> usize {
        self.active_child_refreshes
            .values()
            .filter(|active| active.request.priority == ChildRefreshPriority::Background)
            .count()
    }

    fn start_child_refresh_job(&mut self, request: ChildRefreshRequest) {
        let key = request.key();
        let sender = self.sender.clone();
        let runner = Arc::clone(&self.runner);
        let state_root = self.config.state_root.clone();
        self.active_child_refreshes
            .insert(key, ActiveChildRefresh::new(request.clone()));

        thread::spawn(move || {
            let result = runner.run_virtual_fs_refresh_children(
                state_root,
                request.mount_id.clone(),
                request.container_identifier.clone(),
            );
            let _ = sender.send(RuntimeMessage::JobFinished(
                JobCompletion::VirtualFsRefreshChildren {
                    mount_id: request.mount_id,
                    container_identifier: request.container_identifier,
                    depth: request.depth,
                    priority: request.priority,
                    result,
                },
            ));
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
        if request.reason.is_remote_fast_forward() {
            match self.auto_fast_forward_queue_decision(&request) {
                Ok(AutoFastForwardQueueDecision::Queue) => {}
                Ok(AutoFastForwardQueueDecision::Skip) => return,
                Ok(AutoFastForwardQueueDecision::Delay(job)) => {
                    self.queue_freshness(job);
                    return;
                }
                Err(error) => {
                    eprintln!(
                        "localityd skipped remote fast-forward for `{}`: {error}",
                        request.path.display()
                    );
                    return;
                }
            }
        }

        self.hydration.queue_request(request.clone());

        match SqliteStateStore::open(self.config.state_root.clone())
            .and_then(|mut store| store.upsert_hydration_job(HydrationJobRecord::from(request)))
        {
            Ok(()) => {}
            Err(error) => eprintln!("localityd failed to persist hydration request: {error}"),
        }
    }

    fn queue_freshness(&mut self, job: SyncJob) -> bool {
        if !self.config.background_connector_sync {
            return false;
        }
        self.freshness.upsert(job);
        true
    }

    fn queue_bounded_live_mode_remote_observe(&mut self, job: SyncJob) -> bool {
        if self.freshness.contains(&job)
            || self
                .freshness
                .matching_count(is_live_mode_remote_observe_job)
                < live_mode_remote_observe_queue_budget(self.config.pull_scheduler.active_interval)
        {
            return self.queue_freshness(job);
        }
        false
    }

    fn queue_auto_push(&mut self, target_path: PathBuf) {
        eprintln!(
            "localityd auto-save push queued immediately for `{}`",
            target_path.display()
        );
        self.pending_requests.push_front(MutatingRequest::AutoPush {
            job: auto_push_job(target_path),
        });
        self.maybe_start_next_job();
    }

    fn auto_fast_forward_queue_decision(
        &self,
        request: &HydrationRequest,
    ) -> locality_core::LocalityResult<AutoFastForwardQueueDecision> {
        let store =
            SqliteStateStore::open(self.config.state_root.clone()).map_err(LocalityError::from)?;
        auto_fast_forward_queue_decision(&store, &self.config.state_root, request)
    }

    fn prime_virtual_mounts(&mut self) {
        let mounts = match SqliteStateStore::open(self.config.state_root.clone())
            .and_then(|store| store.load_mounts())
        {
            Ok(mounts) => mounts,
            Err(error) => {
                eprintln!(
                    "localityd failed to load mounts for virtual filesystem priming: {error}"
                );
                return;
            }
        };
        self.completed_child_refreshes.clear();

        for mount in mounts
            .into_iter()
            .filter(|mount| mount.projection.uses_virtual_filesystem())
        {
            self.queue_child_refresh(
                mount.mount_id.0.clone(),
                ROOT_CONTAINER_IDENTIFIER.to_string(),
                ChildRefreshPriority::Background,
                0,
            );
            self.queue_child_refresh(
                mount.mount_id.0.clone(),
                mount_point_identifier(&mount),
                ChildRefreshPriority::Background,
                0,
            );
        }
    }

    fn queue_child_refresh(
        &mut self,
        mount_id: String,
        container_identifier: String,
        priority: ChildRefreshPriority,
        depth: u32,
    ) {
        if !self.config.background_connector_sync {
            return;
        }
        let key = ChildRefreshKey {
            mount_id: mount_id.clone(),
            container_identifier: container_identifier.clone(),
        };
        if self
            .completed_child_refreshes
            .get(&key)
            .is_some_and(|completed_priority| *completed_priority >= priority)
        {
            return;
        }
        if self.active_child_refreshes.contains_key(&key) {
            return;
        }
        let request = ChildRefreshRequest {
            mount_id,
            container_identifier,
            priority,
            depth,
        };
        self.persist_child_refresh(&request);
        self.child_refreshes.queue(request);
        self.maybe_start_next_job();
    }

    fn persist_child_refresh(&self, request: &ChildRefreshRequest) {
        let now = freshness_timestamp();
        let job = MetadataDiscoveryJobRecord {
            mount_id: MountId::new(request.mount_id.clone()),
            container_identifier: request.container_identifier.clone(),
            priority: metadata_discovery_priority_from_child_refresh(request.priority),
            depth: request.depth,
            attempts: 0,
            last_error: None,
            created_at: now.clone(),
            updated_at: now,
        };
        match SqliteStateStore::open(self.config.state_root.clone())
            .and_then(|mut store| store.upsert_metadata_discovery_job(job))
        {
            Ok(()) => {}
            Err(error) => {
                eprintln!("localityd failed to persist metadata discovery request: {error}")
            }
        }
    }

    fn delete_metadata_discovery_job(&self, mount_id: &str, container_identifier: &str) {
        match SqliteStateStore::open(self.config.state_root.clone()).and_then(|mut store| {
            store.delete_metadata_discovery_job(&MountId::new(mount_id), container_identifier)
        }) {
            Ok(()) => {}
            Err(error) => {
                eprintln!(
                    "localityd failed to remove completed metadata discovery request: {error}"
                )
            }
        }
    }

    fn record_metadata_discovery_failure(
        &self,
        mount_id: &str,
        container_identifier: &str,
        message: String,
    ) {
        match SqliteStateStore::open(self.config.state_root.clone()).and_then(|mut store| {
            store.record_metadata_discovery_job_failure(
                &MountId::new(mount_id),
                container_identifier,
                message,
            )
        }) {
            Ok(()) => {}
            Err(error) => {
                eprintln!("localityd failed to record metadata discovery failure: {error}")
            }
        }
    }

    fn mark_child_refresh_completed(
        &mut self,
        mount_id: &str,
        container_identifier: &str,
        priority: ChildRefreshPriority,
    ) {
        let key = ChildRefreshKey {
            mount_id: mount_id.to_string(),
            container_identifier: container_identifier.to_string(),
        };
        self.completed_child_refreshes
            .entry(key)
            .and_modify(|completed_priority| {
                *completed_priority = (*completed_priority).max(priority);
            })
            .or_insert(priority);
    }

    fn delete_hydration_job(&self, request: &HydrationRequest) {
        match SqliteStateStore::open(self.config.state_root.clone())
            .and_then(|mut store| store.delete_hydration_job(&request.mount_id, &request.remote_id))
        {
            Ok(()) => {}
            Err(error) => {
                eprintln!("localityd failed to remove completed hydration request: {error}")
            }
        }
    }

    fn record_hydration_failure(&self, request: &HydrationRequest, message: String) {
        match SqliteStateStore::open(self.config.state_root.clone()).and_then(|mut store| {
            store.record_hydration_job_failure(&request.mount_id, &request.remote_id, message)
        }) {
            Ok(()) => {}
            Err(error) => eprintln!("localityd failed to record hydration failure: {error}"),
        }
    }

    fn refresh_visible_projection_after_remote_fast_forward(
        &self,
        request: &HydrationRequest,
        previous_shadow: Option<&ShadowDocument>,
    ) {
        if !request.reason.is_remote_fast_forward() {
            return;
        }
        let Some(previous_shadow) = previous_shadow else {
            return;
        };

        let store = match SqliteStateStore::open(self.config.state_root.clone()) {
            Ok(store) => store,
            Err(error) => {
                eprintln!(
                    "localityd failed to open state for visible projection refresh after remote fast-forward: {error}"
                );
                return;
            }
        };
        if let Err(error) = file_provider::refresh_visible_entity_projection_if_clean(
            &store,
            &self.config.state_root,
            &request.mount_id,
            &request.remote_id,
            previous_shadow,
        ) {
            eprintln!(
                "localityd failed to refresh visible projection after remote fast-forward for `{}`: {error}",
                request.path.display()
            );
        }
    }

    fn queue_remote_fast_forward_discovery_hints(
        &mut self,
        request: &HydrationRequest,
        previous_shadow: Option<&ShadowDocument>,
    ) {
        if !request.reason.is_remote_fast_forward() {
            return;
        }
        let Some(previous_shadow) = previous_shadow else {
            return;
        };

        let store = match SqliteStateStore::open(self.config.state_root.clone()) {
            Ok(store) => store,
            Err(error) => {
                eprintln!(
                    "localityd failed to open state for remote discovery hints after fast-forward: {error}"
                );
                return;
            }
        };
        let mount = match store.get_mount(&request.mount_id) {
            Ok(Some(mount)) => mount,
            Ok(None) => return,
            Err(error) => {
                eprintln!(
                    "localityd failed to inspect mount for remote discovery hints after fast-forward: {error}"
                );
                return;
            }
        };
        if !mount.projection.uses_virtual_filesystem() {
            return;
        }
        let current_shadow = match store.load_shadow(&request.mount_id, &request.remote_id) {
            Ok(shadow) => shadow,
            Err(error) => {
                eprintln!(
                    "localityd failed to load updated shadow for remote discovery hints after fast-forward: {error}"
                );
                return;
            }
        };

        for hint in remote_fast_forward_discovery_hints(previous_shadow, &current_shadow) {
            match hint {
                RemoteDiscoveryHint::RefreshChildren {
                    container_identifier,
                } => {
                    self.queue_child_refresh(
                        request.mount_id.0.clone(),
                        container_identifier,
                        ChildRefreshPriority::Background,
                        0,
                    );
                }
            }
        }
    }

    /// Debug-only diagnostics for the desktop Activity queue tab.
    ///
    /// This reads the runtime's in-memory queues without touching connector or
    /// store state so it stays cheap and cannot affect scheduling.
    fn debug_queue_status(&self) -> DaemonDebugQueueStatus {
        let now = freshness_timestamp();
        let freshness_metrics = self.freshness.metrics(Some(&now));
        let active = self.debug_active_jobs();
        let mut sections = Vec::new();

        sections.push(debug_notion_transport_section());

        sections.push(DaemonDebugQueueSection {
            name: "pending_requests".to_string(),
            label: "Daemon work queue".to_string(),
            total: self.pending_requests.len(),
            ready: Some(self.pending_requests.len()),
            deferred: None,
            items: self
                .pending_requests
                .iter()
                .take(DEBUG_QUEUE_ITEM_LIMIT)
                .map(debug_mutating_request_item)
                .collect(),
        });

        sections.push(DaemonDebugQueueSection {
            name: "hydrations".to_string(),
            label: "Hydration fetches".to_string(),
            total: self.hydration.len(),
            ready: Some(self.hydration.len()),
            deferred: None,
            items: self
                .hydration
                .debug_requests(DEBUG_QUEUE_ITEM_LIMIT)
                .into_iter()
                .map(debug_hydration_item)
                .collect(),
        });

        sections.push(DaemonDebugQueueSection {
            name: "deferred_hydrations".to_string(),
            label: "Hydration retries".to_string(),
            total: self.deferred_hydration.len(),
            ready: Some(0),
            deferred: Some(self.deferred_hydration.len()),
            items: self
                .deferred_hydration
                .iter()
                .take(DEBUG_QUEUE_ITEM_LIMIT)
                .cloned()
                .map(debug_hydration_item)
                .collect(),
        });

        sections.push(DaemonDebugQueueSection {
            name: "freshness".to_string(),
            label: "Freshness observations".to_string(),
            total: freshness_metrics.total_jobs,
            ready: Some(freshness_metrics.ready_jobs),
            deferred: Some(freshness_metrics.deferred_jobs),
            items: self
                .freshness
                .debug_jobs(Some(&now), DEBUG_QUEUE_ITEM_LIMIT)
                .into_iter()
                .map(debug_freshness_item)
                .collect(),
        });

        sections.push(DaemonDebugQueueSection {
            name: "child_refreshes".to_string(),
            label: "Directory discovery".to_string(),
            total: self.child_refreshes.len(),
            ready: None,
            deferred: None,
            items: self
                .child_refreshes
                .debug_requests(DEBUG_QUEUE_ITEM_LIMIT)
                .into_iter()
                .map(debug_child_refresh_item)
                .collect(),
        });

        sections.push(DaemonDebugQueueSection {
            name: "scheduled_pull".to_string(),
            label: "Scheduled pull".to_string(),
            total: usize::from(self.pending_scheduled_tick.is_some()),
            ready: Some(usize::from(self.pending_scheduled_tick.is_some())),
            deferred: None,
            items: self
                .pending_scheduled_tick
                .as_ref()
                .map(debug_scheduled_pull_item)
                .into_iter()
                .collect(),
        });

        DaemonDebugQueueStatus {
            generated_at_unix_ms: unix_time_ms(),
            active,
            sections,
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

    fn debug_active_jobs(&self) -> Vec<DaemonActiveJobStatus> {
        let mut active = Vec::new();
        if let Some(job) = &self.active_job {
            active.push(job.status());
        }
        active.extend(
            self.active_child_refreshes
                .values()
                .map(|active| active.status.status()),
        );
        active
    }

    fn status(&self) -> DaemonRuntimeStatus {
        let freshness_metrics = self.freshness.metrics(Some(&freshness_timestamp()));
        let active_child_refresh = self
            .active_child_refreshes
            .values()
            .next()
            .map(|active| active.status.status());
        DaemonRuntimeStatus {
            active_job: self.active_job.is_some() || active_child_refresh.is_some(),
            active_job_detail: self
                .active_job
                .as_ref()
                .map(ActiveRuntimeJob::status)
                .or(active_child_refresh),
            pending_requests: self.pending_requests.len() + self.child_refreshes.len(),
            pending_hydrations: self.hydration.len(),
            deferred_hydrations: self.deferred_hydration.len(),
            pending_freshness: freshness_metrics.total_jobs,
            ready_freshness: freshness_metrics.ready_jobs,
            deferred_freshness: freshness_metrics.deferred_jobs,
            freshness_budget_units: freshness_metrics.total_budget_units,
            ready_freshness_budget_units: freshness_metrics.ready_budget_units,
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

fn debug_mutating_request_item(request: &MutatingRequest) -> DaemonDebugQueueItem {
    let (kind, target) = request.active_status_parts();
    DaemonDebugQueueItem {
        kind,
        target,
        mount_id: None,
        remote_id: None,
        path: None,
        reason: None,
        priority: Some("exclusive".to_string()),
        next_eligible_at: None,
    }
}

fn debug_notion_transport_section() -> DaemonDebugQueueSection {
    let status = notion_request_debug_status();
    let active_count = status.active.len();
    let waiting_for_token = status.waiting_for_token;
    let mut items = status
        .active
        .into_iter()
        .map(|request| DaemonDebugQueueItem {
            kind: format!("{} {}", request.method, request.path),
            target: Some(request.path),
            mount_id: None,
            remote_id: None,
            path: None,
            reason: Some(format!("attempt {}", request.attempt)),
            priority: Some(format!(
                "active {}ms, waited {}ms",
                request.elapsed_ms, request.waited_for_token_ms
            )),
            next_eligible_at: None,
        })
        .collect::<Vec<_>>();

    if waiting_for_token > 0 {
        items.push(DaemonDebugQueueItem {
            kind: "waiting_for_notion_rate_limit".to_string(),
            target: Some(format!("{} request(s)", waiting_for_token)),
            mount_id: None,
            remote_id: None,
            path: None,
            reason: status
                .limiter
                .cooldown_remaining_ms
                .map(|ms| format!("cooldown {ms}ms")),
            priority: Some(format!(
                "{:.2}/{:.2} tokens",
                status.limiter.tokens, status.limiter.burst
            )),
            next_eligible_at: None,
        });
    }

    if let Some(last) = status.last_completed {
        items.push(DaemonDebugQueueItem {
            kind: format!("last {} {}", last.method, last.path),
            target: Some(last.status),
            mount_id: None,
            remote_id: None,
            path: None,
            reason: Some(format!("attempt {}", last.attempt)),
            priority: Some(format!("completed in {}ms", last.elapsed_ms)),
            next_eligible_at: Some(format!("unix_ms:{}", last.completed_at_unix_ms)),
        });
    }

    DaemonDebugQueueSection {
        name: "notion_transport".to_string(),
        label: "Notion HTTP transport".to_string(),
        total: active_count + waiting_for_token,
        ready: Some(active_count),
        deferred: Some(waiting_for_token),
        items,
    }
}

fn debug_hydration_item(request: HydrationRequest) -> DaemonDebugQueueItem {
    DaemonDebugQueueItem {
        kind: "hydration".to_string(),
        target: Some(request.path.display().to_string()),
        mount_id: Some(request.mount_id.0),
        remote_id: Some(request.remote_id.0),
        path: Some(request.path.display().to_string()),
        reason: Some(hydration_reason_label(&request.reason).to_string()),
        priority: Some(hydration_priority_label(hydration_priority(&request.reason)).to_string()),
        next_eligible_at: None,
    }
}

fn debug_freshness_item(debug: crate::freshness::FreshnessQueueDebugJob) -> DaemonDebugQueueItem {
    let job = debug.job;
    let target = job
        .remote_id
        .as_ref()
        .map(|remote_id| format!("{}:{}", job.mount_id.as_str(), remote_id.as_str()))
        .or_else(|| Some(job.mount_id.as_str().to_string()));
    DaemonDebugQueueItem {
        kind: format!("{:?}", job.kind),
        target,
        mount_id: Some(job.mount_id.0),
        remote_id: job.remote_id.map(|remote_id| remote_id.0),
        path: None,
        reason: Some(format!("{:?}", job.reason)),
        priority: Some(if debug.ready {
            job.tier.as_str().to_string()
        } else {
            format!("deferred {}", job.tier.as_str())
        }),
        next_eligible_at: job.next_eligible_at,
    }
}

fn debug_child_refresh_item(request: ChildRefreshRequest) -> DaemonDebugQueueItem {
    DaemonDebugQueueItem {
        kind: "virtual_fs_refresh_children".to_string(),
        target: Some(format!(
            "{}:{}",
            request.mount_id, request.container_identifier
        )),
        mount_id: Some(request.mount_id),
        remote_id: None,
        path: None,
        reason: Some(format!("depth {}", request.depth)),
        priority: Some(child_refresh_priority_label(request.priority).to_string()),
        next_eligible_at: None,
    }
}

fn debug_scheduled_pull_item(tick: &PullSchedulerTick) -> DaemonDebugQueueItem {
    let reason = match (tick.poll_active, tick.poll_cold) {
        (true, true) => "active+cold",
        (true, false) => "active",
        (false, true) => "cold",
        (false, false) => "empty",
    };
    DaemonDebugQueueItem {
        kind: "scheduled_pull".to_string(),
        target: None,
        mount_id: None,
        remote_id: None,
        path: None,
        reason: Some(reason.to_string()),
        priority: Some("scheduled".to_string()),
        next_eligible_at: None,
    }
}

fn hydration_reason_label(reason: &HydrationReason) -> &'static str {
    match reason {
        HydrationReason::ExplicitPull => "explicit_pull",
        HydrationReason::FileOpen => "file_open",
        HydrationReason::Policy => "policy",
        HydrationReason::RemoteFastForward => "remote_fast_forward",
        HydrationReason::LiveModeRemoteFastForward => "live_mode_remote_fast_forward",
        HydrationReason::StubRead => "stub_read",
        HydrationReason::Prefetch => "prefetch",
    }
}

fn hydration_priority_label(priority: HydrationPriority) -> &'static str {
    match priority {
        HydrationPriority::Low => "low",
        HydrationPriority::Normal => "normal",
        HydrationPriority::High => "high",
    }
}

fn child_refresh_priority_label(priority: ChildRefreshPriority) -> &'static str {
    match priority {
        ChildRefreshPriority::Background => "background",
        ChildRefreshPriority::Interactive => "interactive",
    }
}

fn signal_macos_file_provider_enumerator(
    mount_id: &str,
    container_identifier: &str,
) -> Result<(), String> {
    signal_macos_file_provider_enumerator_impl(mount_id, container_identifier)
}

#[cfg(target_os = "macos")]
fn signal_macos_file_provider_enumerator_impl(
    mount_id: &str,
    container_identifier: &str,
) -> Result<(), String> {
    let Some(helper) = macos_file_provider_helper_path() else {
        return Err("locality-file-providerctl was not found".to_string());
    };
    let identifier =
        file_provider::macos_file_provider_item_identifier(mount_id, container_identifier);
    run_macos_file_provider_helper_action(&helper, "signal", &identifier)
}

#[cfg(target_os = "macos")]
fn run_macos_file_provider_helper_action(
    helper: &Path,
    action: &str,
    identifier: &str,
) -> Result<(), String> {
    let output = Command::new(&helper)
        .arg(action)
        .arg("--mount-id")
        .arg(file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID)
        .arg("--identifier")
        .arg(identifier)
        .arg("--json")
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let message = serde_json::from_str::<Value>(&stdout)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|message| !message.is_empty())
        .or_else(|| (!stderr.is_empty()).then_some(stderr))
        .or_else(|| (!stdout.is_empty()).then_some(stdout))
        .unwrap_or_else(|| format!("locality-file-providerctl exited with {}", output.status));
    Err(message)
}

#[cfg(not(target_os = "macos"))]
fn signal_macos_file_provider_enumerator_impl(
    _mount_id: &str,
    _container_identifier: &str,
) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_file_provider_helper_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("LOCALITY_FILE_PROVIDERCTL") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Some(path);
        }
    }

    let mut candidates = Vec::new();
    if let Ok(current_exe) = env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        candidates.push(dir.join("locality-file-providerctl"));
    }
    candidates.push(PathBuf::from(
        "/Applications/Locality.app/Contents/MacOS/locality-file-providerctl",
    ));

    candidates.into_iter().find(|path| path.exists())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RemoteDiscoveryHint {
    RefreshChildren { container_identifier: String },
}

fn remote_fast_forward_discovery_hints(
    previous_shadow: &ShadowDocument,
    current_shadow: &ShadowDocument,
) -> Vec<RemoteDiscoveryHint> {
    if child_page_link_ids(previous_shadow) == child_page_link_ids(current_shadow) {
        return Vec::new();
    }

    vec![RemoteDiscoveryHint::RefreshChildren {
        container_identifier: format!("children:{}", current_shadow.entity_id.0),
    }]
}

fn child_page_link_ids(shadow: &ShadowDocument) -> BTreeSet<RemoteId> {
    shadow
        .blocks
        .iter()
        .filter(|block| block.native_kind.as_deref() == Some("child_page"))
        .map(|block| block.remote_id.clone())
        .collect()
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
        Err(error) => eprintln!("localityd failed to load persisted hydration requests: {error}"),
    }

    queue
}

fn load_persisted_child_refreshes(state_root: &Path) -> ChildRefreshQueue {
    let mut queue = ChildRefreshQueue::default();
    match SqliteStateStore::open(state_root.to_path_buf())
        .and_then(|store| store.list_metadata_discovery_jobs())
    {
        Ok(jobs) => {
            for job in jobs {
                queue.queue(ChildRefreshRequest {
                    mount_id: job.mount_id.0,
                    container_identifier: job.container_identifier,
                    priority: child_refresh_priority_from_metadata(job.priority),
                    depth: job.depth,
                });
            }
        }
        Err(error) => {
            eprintln!("localityd failed to load persisted metadata discovery requests: {error}")
        }
    }

    queue
}

fn metadata_discovery_priority_from_child_refresh(
    priority: ChildRefreshPriority,
) -> MetadataDiscoveryPriority {
    match priority {
        ChildRefreshPriority::Background => MetadataDiscoveryPriority::Background,
        ChildRefreshPriority::Interactive => MetadataDiscoveryPriority::Interactive,
    }
}

fn child_refresh_priority_from_metadata(
    priority: MetadataDiscoveryPriority,
) -> ChildRefreshPriority {
    match priority {
        MetadataDiscoveryPriority::Background => ChildRefreshPriority::Background,
        MetadataDiscoveryPriority::Interactive => ChildRefreshPriority::Interactive,
    }
}

fn remote_fast_forward_previous_shadow(
    state_root: &Path,
    request: &HydrationRequest,
) -> Option<ShadowDocument> {
    if !request.reason.is_remote_fast_forward() {
        return None;
    }

    SqliteStateStore::open(state_root.to_path_buf())
        .ok()?
        .load_shadow(&request.mount_id, &request.remote_id)
        .ok()
}

fn hydration_request_for_projection<S>(
    store: &S,
    state_root: &Path,
    request: HydrationRequest,
) -> locality_core::LocalityResult<HydrationRequest>
where
    S: MountRepository + EntityRepository,
{
    let Some(mount) = store
        .get_mount(&request.mount_id)
        .map_err(LocalityError::from)?
    else {
        return Ok(request);
    };
    if !mount.projection.uses_virtual_filesystem() {
        return Ok(request);
    }
    let Some(entity) = store
        .get_entity(&request.mount_id, &request.remote_id)
        .map_err(LocalityError::from)?
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
) -> locality_core::LocalityResult<Option<PathBuf>>
where
    S: MountRepository,
{
    let Some(mount) = store
        .get_mount(&request.mount_id)
        .map_err(LocalityError::from)?
    else {
        return Ok(None);
    };
    Ok(mount
        .projection
        .uses_virtual_filesystem()
        .then(|| virtual_fs_content_root(state_root, &mount.mount_id)))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AutoFastForwardQueueDecision {
    Queue,
    Skip,
    Delay(SyncJob),
}

fn auto_fast_forward_queue_decision<S>(
    store: &S,
    state_root: &Path,
    request: &HydrationRequest,
) -> locality_core::LocalityResult<AutoFastForwardQueueDecision>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + FreshnessStateRepository
        + AutoSaveRepository,
{
    let Some(entity) = store
        .get_entity(&request.mount_id, &request.remote_id)
        .map_err(LocalityError::from)?
    else {
        return Ok(AutoFastForwardQueueDecision::Skip);
    };
    if entity.kind != EntityKind::Page || entity.hydration != HydrationState::Hydrated {
        return Ok(AutoFastForwardQueueDecision::Skip);
    }

    let Some(freshness) = store
        .get_freshness_state(&request.mount_id, &request.remote_id)
        .map_err(LocalityError::from)?
    else {
        return Ok(AutoFastForwardQueueDecision::Skip);
    };
    if !freshness.remote_hint_pending {
        return Ok(AutoFastForwardQueueDecision::Skip);
    }

    let Some(mount) = store
        .get_mount(&request.mount_id)
        .map_err(LocalityError::from)?
    else {
        return Ok(AutoFastForwardQueueDecision::Skip);
    };
    let path = projection_content_path(state_root, &mount, &entity);
    if !hydrated_file_matches_shadow(store, &mount, &entity, &path)? {
        return Ok(AutoFastForwardQueueDecision::Skip);
    }

    let now = freshness_timestamp();
    if let Some(next_eligible_at) = active_lease_until(&freshness, &now) {
        return Ok(AutoFastForwardQueueDecision::Delay(
            SyncJob::new(
                request.mount_id.clone(),
                Some(request.remote_id.clone()),
                SyncJobKind::ObserveEntity,
                ChangeHintKind::RemoteMaybeChanged,
            )
            .next_eligible_at(next_eligible_at),
        ));
    }

    Ok(AutoFastForwardQueueDecision::Queue)
}

fn projection_content_path(
    state_root: &Path,
    mount: &MountConfig,
    entity: &EntityRecord,
) -> PathBuf {
    if mount.projection.uses_virtual_filesystem() {
        virtual_fs_content_root(state_root, &mount.mount_id).join(&entity.path)
    } else {
        mount.root.join(&entity.path)
    }
}

fn active_lease_until(freshness: &FreshnessStateRecord, now: &str) -> Option<String> {
    let now_ms = parse_unix_ms(now)?;
    let active_until = [
        freshness.last_opened_at.as_deref(),
        freshness.last_local_change_at.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(parse_unix_ms)
    .map(|timestamp| timestamp.saturating_add(AUTO_FAST_FORWARD_ACTIVE_LEASE_MS))
    .filter(|active_until| *active_until > now_ms)
    .max()?;

    Some(format!("unix_ms:{active_until}"))
}

fn parse_unix_ms(value: &str) -> Option<u64> {
    value.strip_prefix("unix_ms:")?.parse().ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceFreshnessCandidate {
    mount_id: MountId,
    remote_id: RemoteId,
    path: PathBuf,
    reason: ChangeHintKind,
    tier: FreshnessTier,
    last_checked_at: Option<String>,
}

/// Select bounded per-page freshness checks for desktop workspace virtual mounts.
///
/// Workspace-level virtual projections avoid scheduled full enumeration. This
/// helper uses only already-known local state so active or hydrated pages can
/// still be observed without crawling the workspace.
pub fn workspace_virtual_freshness_jobs<S>(
    store: &S,
    mounts: &[MountConfig],
    tick: &PullSchedulerTick,
) -> locality_core::LocalityResult<Vec<SyncJob>>
where
    S: EntityRepository + FreshnessStateRepository + MountLiveModeRepository,
{
    if tick.is_idle() {
        return Ok(Vec::new());
    }

    let mut candidates = Vec::new();
    let now_ms = freshness_unix_ms();
    let policy = FreshnessOptimizationPolicy::default();
    let live_mode_enabled_by_mount = store
        .list_mount_live_modes()
        .map_err(LocalityError::from)?
        .into_iter()
        .map(|record| (record.mount_id, record.enabled))
        .collect::<BTreeMap<_, _>>();
    for mount in mounts
        .iter()
        .filter(|mount| is_workspace_virtual_mount(mount))
    {
        let live_mode_enabled = live_mode_enabled_by_mount
            .get(&mount.mount_id)
            .copied()
            .unwrap_or(false);
        let freshness_by_remote_id = store
            .list_freshness_states(&mount.mount_id)
            .map_err(LocalityError::from)?
            .into_iter()
            .map(|state| (state.remote_id.clone(), state))
            .collect::<BTreeMap<_, _>>();

        for entity in store
            .list_entities(&mount.mount_id)
            .map_err(LocalityError::from)?
        {
            if !is_workspace_freshness_entity(&entity) {
                continue;
            }

            let freshness = freshness_by_remote_id
                .get(&entity.remote_id)
                .cloned()
                .unwrap_or_else(|| {
                    FreshnessStateRecord::new(
                        entity.mount_id.clone(),
                        entity.remote_id.clone(),
                        default_workspace_freshness_tier(&entity),
                    )
                });
            if workspace_next_check_is_deferred(&freshness, now_ms) {
                continue;
            }
            let optimized =
                optimized_freshness_decision(&freshness, Some(&entity), now_ms, &policy);
            let selected_by_active_tick = tick.poll_active
                && is_active_workspace_freshness_candidate(&entity, &freshness, &optimized.tier);
            let selected_by_cold_tick = tick.poll_cold;
            if !selected_by_active_tick && !selected_by_cold_tick {
                continue;
            }

            let reason = workspace_freshness_reason(&entity, &freshness, selected_by_active_tick);
            let tier = workspace_freshness_tier(
                &entity,
                &optimized.tier,
                &reason,
                live_mode_enabled,
                selected_by_active_tick,
            );
            candidates.push(WorkspaceFreshnessCandidate {
                mount_id: entity.mount_id,
                remote_id: entity.remote_id,
                path: entity.path,
                reason,
                tier,
                last_checked_at: freshness.last_checked_at,
            });
        }
    }

    candidates.sort_by(compare_workspace_freshness_candidates);
    cap_live_mode_active_workspace_candidates(
        &mut candidates,
        live_mode_remote_observe_queue_budget(Duration::from_secs(15)),
    );
    candidates.truncate(MAX_WORKSPACE_FRESHNESS_JOBS_PER_TICK);

    Ok(candidates
        .into_iter()
        .map(|candidate| {
            SyncJob::new(
                candidate.mount_id,
                Some(candidate.remote_id),
                SyncJobKind::ObserveEntity,
                candidate.reason,
            )
            .with_tier(candidate.tier)
        })
        .collect())
}

fn is_workspace_virtual_mount(mount: &MountConfig) -> bool {
    mount.projection.uses_virtual_filesystem() && mount.remote_root_id.is_none()
}

fn is_workspace_freshness_entity(entity: &EntityRecord) -> bool {
    entity.kind == EntityKind::Page
        && matches!(
            entity.hydration,
            HydrationState::Hydrated | HydrationState::Dirty | HydrationState::Conflicted
        )
}

fn is_active_workspace_freshness_candidate(
    entity: &EntityRecord,
    freshness: &FreshnessStateRecord,
    tier: &FreshnessTier,
) -> bool {
    matches!(
        entity.hydration,
        HydrationState::Dirty | HydrationState::Conflicted
    ) || freshness.remote_hint_pending
        || matches!(tier, FreshnessTier::Immediate | FreshnessTier::Hot)
}

fn workspace_freshness_reason(
    entity: &EntityRecord,
    freshness: &FreshnessStateRecord,
    selected_by_active_tick: bool,
) -> ChangeHintKind {
    if matches!(
        entity.hydration,
        HydrationState::Dirty | HydrationState::Conflicted
    ) {
        return ChangeHintKind::LocalEdited;
    }
    if freshness.remote_hint_pending {
        return ChangeHintKind::RemoteMaybeChanged;
    }
    if selected_by_active_tick {
        return ChangeHintKind::FileOpened;
    }
    ChangeHintKind::BackgroundPoll
}

fn workspace_freshness_tier(
    entity: &EntityRecord,
    optimized_tier: &FreshnessTier,
    reason: &ChangeHintKind,
    live_mode_enabled: bool,
    selected_by_active_tick: bool,
) -> FreshnessTier {
    if live_mode_enabled && selected_by_active_tick && matches!(reason, ChangeHintKind::FileOpened)
    {
        return FreshnessTier::Immediate;
    }
    let mut tier = optimized_tier.clone();
    let reason_tier = reason.recommended_tier();
    if reason_tier.is_more_urgent_than(&tier) {
        tier = reason_tier;
    }
    let default_tier = default_workspace_freshness_tier(entity);
    if default_tier.is_more_urgent_than(&tier) {
        tier = default_tier;
    }
    tier
}

fn cap_live_mode_active_workspace_candidates(
    candidates: &mut Vec<WorkspaceFreshnessCandidate>,
    limit: usize,
) {
    let mut active_live_mode_count = 0usize;
    candidates.retain(|candidate| {
        if !is_live_mode_active_workspace_candidate(candidate) {
            return true;
        }
        if active_live_mode_count >= limit {
            return false;
        }
        active_live_mode_count += 1;
        true
    });
}

fn is_live_mode_active_workspace_candidate(candidate: &WorkspaceFreshnessCandidate) -> bool {
    candidate.reason == ChangeHintKind::FileOpened && candidate.tier == FreshnessTier::Immediate
}

fn workspace_next_check_is_deferred(freshness: &FreshnessStateRecord, now_ms: u64) -> bool {
    freshness
        .next_check_at
        .as_deref()
        .and_then(parse_freshness_timestamp)
        .is_some_and(|next_check_at| next_check_at > now_ms)
}

fn default_workspace_freshness_tier(entity: &EntityRecord) -> FreshnessTier {
    match entity.hydration {
        HydrationState::Dirty | HydrationState::Conflicted => FreshnessTier::Hot,
        HydrationState::Hydrated => FreshnessTier::Warm,
        HydrationState::Virtual | HydrationState::Stub => FreshnessTier::Cold,
    }
}

fn compare_workspace_freshness_candidates(
    left: &WorkspaceFreshnessCandidate,
    right: &WorkspaceFreshnessCandidate,
) -> std::cmp::Ordering {
    left.tier
        .cmp(&right.tier)
        .then_with(|| compare_workspace_last_checked(&left.last_checked_at, &right.last_checked_at))
        .then_with(|| left.path.cmp(&right.path))
        .then_with(|| left.mount_id.cmp(&right.mount_id))
        .then_with(|| left.remote_id.cmp(&right.remote_id))
}

fn compare_workspace_last_checked(
    left: &Option<String>,
    right: &Option<String>,
) -> std::cmp::Ordering {
    match (
        left.as_deref().and_then(parse_freshness_timestamp),
        right.as_deref().and_then(parse_freshness_timestamp),
    ) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(left), Some(right)) => left.cmp(&right),
    }
}

fn live_mode_remote_observe_queue_budget(interval: Duration) -> usize {
    live_mode_remote_observe_queue_budget_for_rate(notion_requests_per_second_setting(), interval)
}

fn live_mode_remote_observe_queue_budget_for_rate(
    requests_per_second: f64,
    interval: Duration,
) -> usize {
    if !requests_per_second.is_finite() || requests_per_second <= 0.0 || interval.is_zero() {
        return 1;
    }
    ((requests_per_second * interval.as_secs_f64() * LIVE_MODE_REMOTE_OBSERVE_QUEUE_SHARE)
        .floor()
        .max(1.0) as usize)
        .min(LIVE_MODE_REMOTE_OBSERVE_MAX_QUEUE_JOBS)
}

fn is_live_mode_remote_observe_job(job: &SyncJob) -> bool {
    job.kind == SyncJobKind::ObserveEntity
        && job.reason == ChangeHintKind::RemoteMaybeChanged
        && job.tier == FreshnessTier::Immediate
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
        MutatingJob::Request(MutatingRequest::AutoPush { job }) => {
            let target_path = job.target_path.clone();
            eprintln!(
                "localityd auto-save push started for `{}`",
                target_path.display()
            );
            JobCompletion::AutoPush {
                target_path,
                response: runner.run_auto_push(state_root, job),
            }
        }
        MutatingJob::Request(MutatingRequest::FileEvent { event }) => {
            JobCompletion::FileEvent(runner.run_file_event(state_root, event))
        }
        MutatingJob::Request(MutatingRequest::VirtualFsMaterialize {
            mount_id,
            identifier,
            respond_to,
        }) => {
            let response = runner.run_virtual_fs_materialize(state_root, mount_id, identifier);
            let freshness_jobs = response_observe_jobs(&response, ChangeHintKind::FileOpened);
            JobCompletion::Response {
                response,
                respond_to,
                freshness_jobs,
                auto_push_targets: Vec::new(),
            }
        }
        MutatingJob::Request(MutatingRequest::FileProviderRead {
            mount_id,
            identifier,
            respond_to,
        }) => {
            let response = runner.run_file_provider_read(state_root, mount_id, identifier);
            let freshness_jobs = response_observe_jobs(&response, ChangeHintKind::FileOpened);
            JobCompletion::Response {
                response,
                respond_to,
                freshness_jobs,
                auto_push_targets: Vec::new(),
            }
        }
        MutatingJob::Request(MutatingRequest::VirtualFsCommitWrite {
            mount_id,
            identifier,
            contents_base64,
            respond_to,
        }) => {
            let response = runner.run_virtual_fs_commit_write(
                state_root.clone(),
                mount_id,
                identifier,
                contents_base64,
            );
            let freshness_jobs = response_local_edit_observe_jobs(&response);
            let auto_push_targets = response_auto_push_targets(&state_root, &response);
            JobCompletion::Response {
                response,
                respond_to,
                freshness_jobs,
                auto_push_targets,
            }
        }
        MutatingJob::Request(MutatingRequest::VirtualFsCreateFile {
            mount_id,
            parent_identifier,
            filename,
            respond_to,
        }) => {
            let response = runner.run_virtual_fs_create_file(
                state_root,
                mount_id,
                parent_identifier,
                filename,
            );
            let freshness_jobs = response_observe_jobs(&response, ChangeHintKind::LocalEdited);
            JobCompletion::Response {
                response,
                respond_to,
                freshness_jobs,
                auto_push_targets: Vec::new(),
            }
        }
        MutatingJob::Request(MutatingRequest::VirtualFsCreateDirectory {
            mount_id,
            parent_identifier,
            dirname,
            respond_to,
        }) => {
            let response = runner.run_virtual_fs_create_directory(
                state_root,
                mount_id,
                parent_identifier,
                dirname,
            );
            let freshness_jobs = response_observe_jobs(&response, ChangeHintKind::LocalEdited);
            JobCompletion::Response {
                response,
                respond_to,
                freshness_jobs,
                auto_push_targets: Vec::new(),
            }
        }
        MutatingJob::Request(MutatingRequest::VirtualFsRename {
            mount_id,
            identifier,
            new_parent_identifier,
            new_filename,
            respond_to,
        }) => {
            let response = runner.run_virtual_fs_rename(
                state_root,
                mount_id,
                identifier,
                new_parent_identifier,
                new_filename,
            );
            let freshness_jobs = response_observe_jobs(&response, ChangeHintKind::LocalEdited);
            JobCompletion::Response {
                response,
                respond_to,
                freshness_jobs,
                auto_push_targets: Vec::new(),
            }
        }
        MutatingJob::Request(MutatingRequest::VirtualFsTrash {
            mount_id,
            identifier,
            respond_to,
        }) => {
            let response = runner.run_virtual_fs_trash(state_root, mount_id, identifier);
            let freshness_jobs = response_observe_jobs(&response, ChangeHintKind::LocalEdited);
            JobCompletion::Response {
                response,
                respond_to,
                freshness_jobs,
                auto_push_targets: Vec::new(),
            }
        }
        MutatingJob::ScheduledPull { tick } => {
            JobCompletion::ScheduledPull(runner.run_scheduled_pull(state_root, tick, policy))
        }
        MutatingJob::Hydration { request } => {
            let previous_shadow = remote_fast_forward_previous_shadow(&state_root, &request);
            let result = runner.run_hydration(state_root, request.clone());
            JobCompletion::Hydration {
                request,
                result,
                previous_shadow,
            }
        }
        MutatingJob::Freshness { job } => {
            JobCompletion::Freshness(runner.run_freshness_job(state_root, job))
        }
    }
}

fn response_local_edit_observe_jobs(response: &DaemonResponse) -> Vec<SyncJob> {
    if response
        .payload
        .as_ref()
        .and_then(|payload| payload.get("hydration"))
        .and_then(Value::as_str)
        == Some("hydrated")
    {
        return Vec::new();
    }

    response_observe_jobs(response, ChangeHintKind::LocalEdited)
}

fn response_auto_push_targets(state_root: &Path, response: &DaemonResponse) -> Vec<PathBuf> {
    if !response.ok {
        return Vec::new();
    }

    let Some(payload) = response.payload.as_ref() else {
        return Vec::new();
    };
    if payload
        .get("hydration")
        .and_then(Value::as_str)
        .is_some_and(|hydration| hydration != "dirty")
    {
        return Vec::new();
    }

    let Some(mount_id) = payload.get("mount_id").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(remote_id) = payload.get("remote_id").and_then(Value::as_str) else {
        return Vec::new();
    };
    if !observable_remote_identifier(remote_id) {
        return Vec::new();
    }

    let Ok(store) = SqliteStateStore::open(state_root.to_path_buf()) else {
        return Vec::new();
    };
    let mount_id = MountId::new(mount_id);
    let remote_id = RemoteId::new(remote_id);
    let Ok(Some(enrollment)) = store.find_auto_save_enrollment_by_remote_id(&mount_id, &remote_id)
    else {
        return Vec::new();
    };
    if !enrollment.enabled
        || matches!(
            enrollment.state,
            AutoSaveState::PausedRemoteChanged | AutoSaveState::PausedFailure
        )
    {
        return Vec::new();
    }
    let Ok(Some(mount)) = store.get_mount(&mount_id) else {
        return Vec::new();
    };

    vec![mount.root.join(enrollment.path)]
}

fn auto_push_job(target_path: PathBuf) -> PushJob {
    PushJob {
        target_path,
        assume_yes: true,
        confirm_dangerous: false,
    }
}

fn response_observe_jobs(response: &DaemonResponse, reason: ChangeHintKind) -> Vec<SyncJob> {
    if !response.ok {
        return Vec::new();
    }

    let Some(payload) = response.payload.as_ref() else {
        return Vec::new();
    };
    let Some(mount_id) = payload.get("mount_id").and_then(Value::as_str) else {
        return Vec::new();
    };
    let identifier = payload.get("identifier").and_then(Value::as_str);
    let remote_id = payload
        .get("remote_id")
        .and_then(Value::as_str)
        .or(identifier);
    let Some(remote_id) = remote_id.filter(|remote_id| {
        identifier.is_none_or(observable_remote_identifier)
            && observable_remote_identifier(remote_id)
    }) else {
        return Vec::new();
    };

    vec![SyncJob::new(
        MountId::new(mount_id),
        Some(RemoteId::new(remote_id)),
        SyncJobKind::ObserveEntity,
        reason,
    )]
}

fn observable_remote_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && !identifier.starts_with("local:")
        && !identifier.starts_with("schema:")
        && !identifier.starts_with("children:")
        && !identifier.starts_with("guidance:")
        && !identifier.starts_with("path:")
        && !identifier.starts_with(MOUNT_POINT_PREFIX)
        && !identifier.starts_with("source:")
        && identifier != ROOT_CONTAINER_IDENTIFIER
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
    PrimeVirtualMounts,
    JobFinished(JobCompletion),
    Shutdown,
}

enum RuntimeLoopDecision {
    Continue,
    Stop,
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
    AutoPush {
        job: PushJob,
    },
    FileEvent {
        event: FileEvent,
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
    VirtualFsCreateDirectory {
        mount_id: String,
        parent_identifier: String,
        dirname: String,
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
    Freshness { job: SyncJob },
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
            Self::Freshness { job } => (
                "freshness".to_string(),
                Some(match job.remote_id.as_ref() {
                    Some(remote_id) => format!("{}:{}", job.mount_id.as_str(), remote_id.as_str()),
                    None => job.mount_id.as_str().to_string(),
                }),
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
            Self::AutoPush { job } => (
                "auto_push".to_string(),
                Some(job.target_path.display().to_string()),
            ),
            Self::FileEvent { event } => (
                "file_event".to_string(),
                Some(event.path.display().to_string()),
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
            Self::VirtualFsCreateDirectory {
                mount_id,
                parent_identifier,
                dirname,
                ..
            } => (
                "virtual_fs_create_directory".to_string(),
                Some(format!("{mount_id}:{parent_identifier}/{dirname}")),
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
    AutoPush {
        target_path: PathBuf,
        response: DaemonResponse,
    },
    Response {
        response: DaemonResponse,
        respond_to: Sender<DaemonResponse>,
        freshness_jobs: Vec<SyncJob>,
        auto_push_targets: Vec<PathBuf>,
    },
    VirtualFsRefreshChildren {
        mount_id: String,
        container_identifier: String,
        depth: u32,
        priority: ChildRefreshPriority,
        result: locality_core::LocalityResult<VirtualFsRefreshChildrenReport>,
    },
    ScheduledPull(locality_core::LocalityResult<ScheduledPullRuntimeReport>),
    Hydration {
        request: HydrationRequest,
        result: locality_core::LocalityResult<HydrationOutcome>,
        previous_shadow: Option<ShadowDocument>,
    },
    FileEvent(locality_core::LocalityResult<FileEventRuntimeReport>),
    Freshness(locality_core::LocalityResult<FreshnessRuntimeReport>),
}

fn execute_file_event<S>(
    store: &mut S,
    event: FileEvent,
) -> locality_core::LocalityResult<FileEventRuntimeReport>
where
    S: MountRepository
        + EntityRepository
        + ShadowRepository
        + FreshnessStateRepository
        + AutoSaveRepository
        + MountLiveModeRepository,
{
    let mut runtime_report = FileEventRuntimeReport::default();
    let Some((mount, entity)) = resolve_event_entity(store, &event.path)? else {
        runtime_report.report.ignored_events = 1;
        return Ok(runtime_report);
    };

    match event.kind {
        FileEventKind::Read => {
            handle_read_event(store, mount, entity, &mut runtime_report)?;
        }
        FileEventKind::Write => {
            handle_write_event(store, mount, entity, event.path, &mut runtime_report)?;
        }
        FileEventKind::Rename | FileEventKind::Remove => runtime_report.report.ignored_events = 1,
    }

    Ok(runtime_report)
}

fn handle_read_event<S>(
    store: &mut S,
    mount: MountConfig,
    entity: EntityRecord,
    runtime_report: &mut FileEventRuntimeReport,
) -> locality_core::LocalityResult<()>
where
    S: FreshnessStateRepository,
{
    if entity.kind == EntityKind::Page {
        record_file_opened(store, &entity)?;
        runtime_report
            .freshness_jobs
            .push(observe_entity_job(&entity, ChangeHintKind::FileOpened));
    }

    if !should_hydrate_on_read(&entity) {
        if runtime_report.freshness_jobs.is_empty() {
            runtime_report.report.ignored_events = 1;
        }
        return Ok(());
    }

    runtime_report.queued_hydrations.push(HydrationRequest::new(
        mount.mount_id.clone(),
        entity.remote_id,
        mount.root.join(&entity.path),
        HydrationState::Hydrated,
        HydrationReason::StubRead,
    ));
    runtime_report.report.queued_hydrations = 1;
    Ok(())
}

fn handle_write_event<S>(
    store: &mut S,
    mount: MountConfig,
    mut entity: EntityRecord,
    event_path: PathBuf,
    runtime_report: &mut FileEventRuntimeReport,
) -> locality_core::LocalityResult<()>
where
    S: EntityRepository
        + ShadowRepository
        + FreshnessStateRepository
        + AutoSaveRepository
        + MountLiveModeRepository,
{
    if entity.hydration != HydrationState::Hydrated {
        if matches!(
            entity.hydration,
            HydrationState::Dirty | HydrationState::Conflicted
        ) {
            record_local_change(store, &entity)?;
            runtime_report
                .freshness_jobs
                .push(observe_entity_job(&entity, ChangeHintKind::LocalEdited));
            if entity.hydration == HydrationState::Dirty
                && let Some(target) =
                    auto_save_target_for_write(store, &mount, &entity, &event_path)?
            {
                runtime_report.auto_push_targets.push(target);
            }
        } else {
            runtime_report.report.ignored_events = 1;
        }
        return Ok(());
    }

    if hydrated_file_matches_shadow(store, &mount, &entity, &event_path)? {
        runtime_report.report.ignored_events = 1;
        return Ok(());
    }

    entity.hydration = HydrationState::Dirty;
    store
        .save_entity(entity.clone())
        .map_err(LocalityError::from)?;
    record_local_change(store, &entity)?;
    runtime_report
        .freshness_jobs
        .push(observe_entity_job(&entity, ChangeHintKind::LocalEdited));
    if let Some(target) = auto_save_target_for_write(store, &mount, &entity, &event_path)? {
        runtime_report.auto_push_targets.push(target);
    }
    runtime_report.report.marked_dirty = 1;
    Ok(())
}

fn observe_entity_job(entity: &EntityRecord, reason: ChangeHintKind) -> SyncJob {
    SyncJob::new(
        entity.mount_id.clone(),
        Some(entity.remote_id.clone()),
        SyncJobKind::ObserveEntity,
        reason,
    )
}

fn execute_freshness_job<S, C>(
    store: &mut S,
    connector: &C,
    job: SyncJob,
) -> locality_core::LocalityResult<FreshnessRuntimeReport>
where
    S: EntityRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + MountLiveModeRepository
        + AutoSaveRepository
        + MountRepository
        + ShadowRepository,
    C: Connector,
{
    if job.kind != SyncJobKind::ObserveEntity {
        return Err(LocalityError::Unsupported(
            "only observe_entity freshness jobs are wired into the daemon runtime",
        ));
    }
    execute_observe_entity_job(store, connector, job)
}

fn execute_observe_entity_job<S, C>(
    store: &mut S,
    connector: &C,
    job: SyncJob,
) -> locality_core::LocalityResult<FreshnessRuntimeReport>
where
    S: EntityRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + MountLiveModeRepository
        + AutoSaveRepository
        + MountRepository
        + ShadowRepository,
    C: Connector,
{
    let Some(remote_id) = job.remote_id.clone() else {
        return Err(LocalityError::InvalidState(
            "observe_entity freshness jobs require a remote id".to_string(),
        ));
    };

    let observation = connector.observe(ObserveRequest {
        mount_id: job.mount_id.clone(),
        remote_id: remote_id.clone(),
    })?;
    apply_remote_observation(store, job, observation)
}

#[doc(hidden)]
pub fn apply_remote_observation<S>(
    store: &mut S,
    job: SyncJob,
    observation: RemoteObservation,
) -> locality_core::LocalityResult<FreshnessRuntimeReport>
where
    S: EntityRepository
        + RemoteObservationRepository
        + FreshnessStateRepository
        + MountLiveModeRepository
        + AutoSaveRepository
        + MountRepository
        + ShadowRepository,
{
    let Some(remote_id) = job.remote_id.clone() else {
        return Err(LocalityError::InvalidState(
            "remote observation jobs require a remote id".to_string(),
        ));
    };
    if observation.mount_id != job.mount_id || observation.remote_id != remote_id {
        return Err(LocalityError::InvalidState(format!(
            "remote observation `{}/{}` does not match job `{}/{}`",
            observation.mount_id.as_str(),
            observation.remote_id.as_str(),
            job.mount_id.as_str(),
            remote_id.as_str()
        )));
    }

    let existing = store
        .get_entity(&job.mount_id, &remote_id)
        .map_err(LocalityError::from)?;
    let existing_freshness = store
        .get_freshness_state(&job.mount_id, &remote_id)
        .map_err(LocalityError::from)?;
    if observation.deleted {
        match remote_delete_policy(
            store,
            None,
            &job.mount_id,
            existing.as_ref(),
            existing_freshness.as_ref(),
        )? {
            RemoteDeletePolicy::RemoveMetadata(_reason) => {
                delete_remote_deleted_state(store, &job.mount_id, &remote_id)?;
                return Ok(FreshnessRuntimeReport {
                    job,
                    remote_hint_pending: false,
                    queued_hydrations: Vec::new(),
                    follow_up_jobs: Vec::new(),
                });
            }
            RemoteDeletePolicy::RemoveCleanProjection {
                path,
                visible_path,
                reason: _,
            } => {
                remove_clean_remote_deleted_projection(&path)?;
                if let Some(visible_path) = visible_path
                    && visible_path != path
                {
                    remove_clean_remote_deleted_projection(&visible_path)?;
                }
                delete_remote_deleted_state(store, &job.mount_id, &remote_id)?;
                return Ok(FreshnessRuntimeReport {
                    job,
                    remote_hint_pending: false,
                    queued_hydrations: Vec::new(),
                    follow_up_jobs: Vec::new(),
                });
            }
            RemoteDeletePolicy::PreserveForReview(_reason) => {}
        }
    }

    let remote_hint_pending = observed_remote_version_changed(existing.as_ref(), &observation);
    let observed_at = freshness_timestamp();
    if remote_hint_pending {
        pause_auto_save_for_remote_change(store, &job.mount_id, &remote_id)?;
    }

    store
        .save_remote_observation(remote_observation_record(&observation, &observed_at))
        .map_err(LocalityError::from)?;

    let mut freshness = existing_freshness.unwrap_or_else(|| {
        FreshnessStateRecord::new(job.mount_id.clone(), remote_id.clone(), job.tier.clone())
    });
    if job.tier.is_more_urgent_than(&freshness.tier) {
        freshness.tier = job.tier.clone();
    }
    freshness.last_checked_at = Some(observed_at);
    freshness.remote_hint_pending = next_remote_hint_pending(
        existing.as_ref(),
        &observation,
        freshness.remote_hint_pending,
        remote_hint_pending,
    );
    store
        .save_freshness_state(freshness)
        .map_err(LocalityError::from)?;

    let queued_hydrations = auto_fast_forward_requests_from_observation(
        store,
        existing.as_ref(),
        &observation,
        remote_hint_pending,
    )?;

    Ok(FreshnessRuntimeReport {
        job,
        remote_hint_pending,
        queued_hydrations,
        follow_up_jobs: Vec::new(),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteDeleteRepairReport {
    pub metadata_removed: usize,
    pub projections_removed: usize,
    pub preserved_for_review: usize,
}

pub fn repair_clean_remote_deleted_projections<S>(
    store: &mut S,
    state_root: Option<&Path>,
    mount_id: Option<&MountId>,
) -> locality_core::LocalityResult<RemoteDeleteRepairReport>
where
    S: MountRepository
        + EntityRepository
        + FreshnessStateRepository
        + RemoteObservationRepository
        + ShadowRepository,
{
    let mount_ids = match mount_id {
        Some(mount_id) => vec![mount_id.clone()],
        None => store
            .load_mounts()
            .map_err(LocalityError::from)?
            .into_iter()
            .map(|mount| mount.mount_id)
            .collect(),
    };
    let mut report = RemoteDeleteRepairReport::default();

    for mount_id in mount_ids {
        let observations = store
            .list_remote_observations(&mount_id)
            .map_err(LocalityError::from)?;
        for observation in observations
            .into_iter()
            .filter(|observation| observation.deleted)
        {
            let remote_id = observation.remote_id.clone();
            let existing = store
                .get_entity(&mount_id, &remote_id)
                .map_err(LocalityError::from)?;
            let freshness = store
                .get_freshness_state(&mount_id, &remote_id)
                .map_err(LocalityError::from)?;

            match remote_delete_policy(
                store,
                state_root,
                &mount_id,
                existing.as_ref(),
                freshness.as_ref(),
            )? {
                RemoteDeletePolicy::RemoveMetadata(_reason) => {
                    delete_remote_deleted_state(store, &mount_id, &remote_id)?;
                    report.metadata_removed += 1;
                }
                RemoteDeletePolicy::RemoveCleanProjection {
                    path,
                    visible_path,
                    reason: _,
                } => {
                    remove_clean_remote_deleted_projection(&path)?;
                    if let Some(visible_path) = visible_path
                        && visible_path != path
                    {
                        remove_clean_remote_deleted_projection(&visible_path)?;
                    }
                    delete_remote_deleted_state(store, &mount_id, &remote_id)?;
                    report.projections_removed += 1;
                }
                RemoteDeletePolicy::PreserveForReview(_reason) => {
                    report.preserved_for_review += 1;
                }
            }
        }
    }

    Ok(report)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RemoteDeletePolicy {
    RemoveMetadata(RemoteDeleteReason),
    RemoveCleanProjection {
        path: PathBuf,
        visible_path: Option<PathBuf>,
        reason: RemoteDeleteReason,
    },
    PreserveForReview(RemoteDeleteReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteDeleteReason {
    UnopenedOnlineOnly,
    CleanHydratedPlainFile,
    UnknownEntity,
    LocalPendingOrDirty,
    MissingMount,
    UnsupportedProjection,
    UnsupportedEntityState,
}

fn remote_delete_policy<S>(
    store: &S,
    state_root: Option<&Path>,
    mount_id: &MountId,
    existing: Option<&EntityRecord>,
    freshness: Option<&FreshnessStateRecord>,
) -> locality_core::LocalityResult<RemoteDeletePolicy>
where
    S: MountRepository + ShadowRepository,
{
    let Some(entity) = existing else {
        return Ok(RemoteDeletePolicy::PreserveForReview(
            RemoteDeleteReason::UnknownEntity,
        ));
    };

    if should_auto_delete_unopened_remote_delete(entity, freshness) {
        return Ok(RemoteDeletePolicy::RemoveMetadata(
            RemoteDeleteReason::UnopenedOnlineOnly,
        ));
    }

    if entity.kind != EntityKind::Page || entity.hydration != HydrationState::Hydrated {
        return Ok(RemoteDeletePolicy::PreserveForReview(
            RemoteDeleteReason::UnsupportedEntityState,
        ));
    }
    if freshness.is_some_and(|state| state.last_local_change_at.is_some()) {
        return Ok(RemoteDeletePolicy::PreserveForReview(
            RemoteDeleteReason::LocalPendingOrDirty,
        ));
    }

    let Some(mount) = store.get_mount(mount_id).map_err(LocalityError::from)? else {
        return Ok(RemoteDeletePolicy::PreserveForReview(
            RemoteDeleteReason::MissingMount,
        ));
    };
    if mount.projection.uses_virtual_filesystem() {
        let Some(state_root) = state_root else {
            return Ok(RemoteDeletePolicy::PreserveForReview(
                RemoteDeleteReason::UnsupportedProjection,
            ));
        };
        let path = projection_content_path(state_root, &mount, entity);
        let visible_path = mount.root.join(&entity.path);
        let visible_is_safe = !visible_path.exists()
            || hydrated_file_is_clean_for_remote_delete(store, &mount, entity, &visible_path)?;
        if visible_is_safe
            && hydrated_file_is_clean_for_remote_delete(store, &mount, entity, &path)?
        {
            return Ok(RemoteDeletePolicy::RemoveCleanProjection {
                path,
                visible_path: Some(visible_path),
                reason: RemoteDeleteReason::CleanHydratedPlainFile,
            });
        }
        return Ok(RemoteDeletePolicy::PreserveForReview(
            RemoteDeleteReason::LocalPendingOrDirty,
        ));
    }

    let path = mount.root.join(&entity.path);
    if hydrated_file_is_clean_for_remote_delete(store, &mount, entity, &path)? {
        return Ok(RemoteDeletePolicy::RemoveCleanProjection {
            path,
            visible_path: None,
            reason: RemoteDeleteReason::CleanHydratedPlainFile,
        });
    }
    Ok(RemoteDeletePolicy::PreserveForReview(
        RemoteDeleteReason::LocalPendingOrDirty,
    ))
}

fn hydrated_file_is_clean_for_remote_delete<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    path: &Path,
) -> locality_core::LocalityResult<bool>
where
    S: ShadowRepository,
{
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(_) => return Ok(false),
    };
    let parsed = match parse_canonical_markdown(&contents) {
        Ok(parsed) => parsed,
        Err(_) => return Ok(false),
    };
    if parsed
        .remote_id()
        .is_some_and(|remote_id| remote_id != &entity.remote_id)
    {
        return Ok(false);
    }
    let shadow = match store.load_shadow(&mount.mount_id, &entity.remote_id) {
        Ok(shadow) => shadow,
        Err(_) => return Ok(false),
    };

    let body_equivalent = rendered_bodies_equivalent(&parsed.document.body, &shadow.rendered_body);
    let plan = BlockDiffEngine::new()
        .with_edited_body_start_line(parsed.body_start_line)
        .plan_push(&shadow, &parsed.document);
    let has_frontmatter_changes = plan
        .as_ref()
        .map(|plan| {
            plan.operations
                .iter()
                .any(|operation| matches!(operation, PushOperation::UpdateProperties { .. }))
        })
        .unwrap_or(false);
    let body_clean = body_equivalent
        || plan
            .as_ref()
            .map(|plan| {
                plan.degradations.is_empty()
                    && plan.operations.iter().all(|operation| {
                        matches!(operation, PushOperation::UpdateProperties { .. })
                    })
            })
            .unwrap_or(false);

    Ok(body_clean && !has_frontmatter_changes)
}

fn delete_remote_deleted_state<S>(
    store: &mut S,
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> locality_core::LocalityResult<()>
where
    S: EntityRepository + FreshnessStateRepository + RemoteObservationRepository,
{
    store
        .delete_entity(mount_id, remote_id)
        .map_err(LocalityError::from)?;
    store
        .delete_freshness_state(mount_id, remote_id)
        .map_err(LocalityError::from)?;
    store
        .delete_remote_observation(mount_id, remote_id)
        .map_err(LocalityError::from)?;
    Ok(())
}

fn remove_clean_remote_deleted_projection(path: &Path) -> locality_core::LocalityResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(LocalityError::Io(format!(
                "failed to remove remote-deleted clean projection `{}`: {error}",
                path.display()
            )));
        }
    }

    if path.file_name().is_some_and(|name| name == "page.md")
        && let Some(directory) = path.parent()
    {
        let _ = std::fs::remove_dir(directory);
    }
    Ok(())
}

fn auto_fast_forward_requests_from_observation<S>(
    store: &S,
    existing: Option<&EntityRecord>,
    observation: &RemoteObservation,
    remote_hint_pending: bool,
) -> locality_core::LocalityResult<Vec<HydrationRequest>>
where
    S: MountLiveModeRepository,
{
    let Some(entity) = existing else {
        return Ok(Vec::new());
    };
    if !remote_hint_pending
        || observation.deleted
        || observation.kind != EntityKind::Page
        || entity.hydration != HydrationState::Hydrated
    {
        return Ok(Vec::new());
    }
    let reason = if mount_live_mode_is_enabled(store, &observation.mount_id)? {
        HydrationReason::LiveModeRemoteFastForward
    } else {
        HydrationReason::RemoteFastForward
    };

    Ok(vec![HydrationRequest::new(
        observation.mount_id.clone(),
        observation.remote_id.clone(),
        entity.path.clone(),
        HydrationState::Hydrated,
        reason,
    )])
}

fn mount_live_mode_is_enabled<S>(
    store: &S,
    mount_id: &MountId,
) -> locality_core::LocalityResult<bool>
where
    S: MountLiveModeRepository,
{
    Ok(store
        .get_mount_live_mode(mount_id)
        .map_err(LocalityError::from)?
        .is_some_and(|record| record.enabled))
}

fn should_auto_delete_unopened_remote_delete(
    entity: &EntityRecord,
    freshness: Option<&FreshnessStateRecord>,
) -> bool {
    matches!(
        entity.hydration,
        HydrationState::Virtual | HydrationState::Stub
    ) && freshness
        .is_none_or(|state| state.last_opened_at.is_none() && state.last_local_change_at.is_none())
}

fn next_remote_hint_pending(
    existing: Option<&EntityRecord>,
    observation: &RemoteObservation,
    previous_pending: bool,
    remote_hint_pending: bool,
) -> bool {
    remote_hint_pending
        || (previous_pending && !observation_matches_synced_version(existing, observation))
}

fn observation_matches_synced_version(
    existing: Option<&EntityRecord>,
    observation: &RemoteObservation,
) -> bool {
    if observation.deleted {
        return false;
    }
    match (
        existing.and_then(EntityRecord::synced_tree_remote_version),
        observation
            .remote_version
            .as_ref()
            .map(|remote_version| remote_version.as_str()),
    ) {
        (Some(synced), Some(observed)) => synced == observed,
        _ => false,
    }
}

fn remote_observation_record(
    observation: &RemoteObservation,
    observed_at: &str,
) -> RemoteObservationRecord {
    let mut record = RemoteObservationRecord::new(
        observation.mount_id.clone(),
        observation.remote_id.clone(),
        observation.kind.clone(),
        observation.title.clone(),
        observation.projected_path.clone(),
        observed_at,
    );
    if let Some(parent_remote_id) = observation.parent_remote_id.clone() {
        record = record.with_parent(parent_remote_id);
    }
    if let Some(remote_version) = observation.remote_version.clone() {
        record = record.with_remote_version(remote_version);
    }
    record
        .deleted(observation.deleted)
        .with_raw_metadata_json(observation.raw_metadata_json.clone())
}

fn observed_remote_version_changed(
    existing: Option<&EntityRecord>,
    observation: &RemoteObservation,
) -> bool {
    if observation.deleted {
        return true;
    }

    match (
        existing.and_then(|record| record.remote_edited_at.as_ref()),
        observation
            .remote_version
            .as_ref()
            .map(|remote_version| remote_version.as_str()),
    ) {
        (Some(base), Some(observed)) => base != observed,
        _ => false,
    }
}

fn hydrated_file_matches_shadow<S>(
    store: &S,
    mount: &MountConfig,
    entity: &EntityRecord,
    event_path: &std::path::Path,
) -> locality_core::LocalityResult<bool>
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

    Ok(parsed_matches_shadow(&parsed, &shadow))
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
) -> locality_core::LocalityResult<Option<(MountConfig, EntityRecord)>>
where
    S: MountRepository + EntityRepository,
{
    let mounts = store.load_mounts().map_err(LocalityError::from)?;
    for mount in &mounts {
        let Some(relative_path) = event_relative_path(&mount.root, event_path) else {
            continue;
        };
        if relative_path.as_os_str().is_empty() {
            continue;
        }

        if let Some(entity) = store
            .find_entity_by_path(&mount.mount_id, &relative_path)
            .map_err(LocalityError::from)?
        {
            return Ok(Some((mount.clone(), entity)));
        }
    }

    if mounts.len() == 1
        && event_path.is_relative()
        && let Some(mount) = mounts.first()
        && let Some(entity) = store
            .find_entity_by_path(&mount.mount_id, event_path)
            .map_err(LocalityError::from)?
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

fn locality_error_code(error: &LocalityError) -> &'static str {
    match error {
        LocalityError::Validation(_) => "validation_failed",
        LocalityError::Conflict(_) => "conflict",
        LocalityError::Guardrail(_) => "guardrail",
        LocalityError::RemoteNotFound(_) => "remote_not_found",
        LocalityError::InvalidState(_) => "invalid_state",
        LocalityError::Unsupported(_) => "unsupported",
        LocalityError::NotImplemented(_) => "not_implemented",
        LocalityError::Io(_) => "io_error",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use locality_core::LocalityError;
    use locality_core::canonical::render_canonical_markdown;
    use locality_core::freshness::{
        ChangeHintKind, FreshnessTier, RemoteObservation, RemoteVersion, SyncJob, SyncJobKind,
    };
    use locality_core::model::{
        CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, SourceSpan,
    };
    use locality_core::shadow::{MarkdownBlockKind, ShadowBlock, ShadowDocument};
    use locality_store::{
        AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, AutoSaveState, ConnectionId,
        ConnectionRecord, ConnectionRepository, EntityRecord, EntityRepository,
        FreshnessStateRecord, FreshnessStateRepository, InMemoryStateStore,
        MetadataDiscoveryJobRecord, MetadataDiscoveryJobRepository, MetadataDiscoveryPriority,
        MountConfig, MountLiveModeRecord, MountLiveModeRepository, MountRepository, ProjectionMode,
        RemoteObservationRecord, RemoteObservationRepository, ShadowRepository, SqliteStateStore,
    };

    use crate::virtual_fs::VirtualFsRefreshChildrenReport;
    use crate::watcher::{FileEvent, FileEventKind};

    use super::{
        ActiveChildRefresh, ActiveRuntimeJob, ChildRefreshPriority, ChildRefreshQueue,
        ChildRefreshRequest, DaemonRequest, DefaultRuntimeJobRunner, JobCompletion,
        RemoteDiscoveryHint, RuntimeJobRunner, RuntimeState, execute_file_event,
        execute_observe_entity_job, observable_remote_identifier,
        remote_fast_forward_discovery_hints, repair_clean_remote_deleted_projections,
    };

    #[test]
    fn child_refresh_queue_promotes_existing_requests() {
        let mut queue = ChildRefreshQueue::default();

        assert!(queue.queue(request(
            "notion-main",
            "children:page-1",
            ChildRefreshPriority::Background,
            0,
        )));
        assert!(queue.queue(request(
            "notion-main",
            "children:page-2",
            ChildRefreshPriority::Background,
            0,
        )));
        assert!(!queue.queue(request(
            "notion-main",
            "children:page-1",
            ChildRefreshPriority::Interactive,
            0,
        )));

        let active = BTreeMap::new();
        let first = queue.pop_ready(&active).expect("first refresh");
        assert_eq!(first.container_identifier, "children:page-1");
        assert_eq!(first.priority, ChildRefreshPriority::Interactive);

        let second = queue.pop_ready(&active).expect("second refresh");
        assert_eq!(second.container_identifier, "children:page-2");
        assert_eq!(second.priority, ChildRefreshPriority::Background);
    }

    #[test]
    fn child_refresh_queue_blocks_deeper_background_while_shallower_is_active() {
        let mut queue = ChildRefreshQueue::default();
        let active_request = request(
            "notion-main",
            "children:page-a",
            ChildRefreshPriority::Background,
            1,
        );
        let mut active = BTreeMap::new();
        active.insert(
            active_request.key(),
            ActiveChildRefresh::new(active_request),
        );

        queue.queue(request(
            "notion-main",
            "children:page-a1",
            ChildRefreshPriority::Background,
            2,
        ));
        queue.queue(request(
            "notion-main",
            "children:page-b",
            ChildRefreshPriority::Background,
            1,
        ));

        let next = queue
            .pop_ready(&active)
            .expect("same-depth sibling refresh");
        assert_eq!(next.container_identifier, "children:page-b");
        assert_eq!(next.depth, 1);

        assert!(
            queue.pop_ready(&active).is_none(),
            "deeper refresh should wait for active shallower work"
        );
        active.clear();
        let deeper = queue.pop_ready(&active).expect("deeper refresh");
        assert_eq!(deeper.container_identifier, "children:page-a1");
    }

    #[test]
    fn metadata_discovery_jobs_reload_into_child_refresh_queue() {
        let state_root = temp_runtime_root("runtime-metadata-discovery-reload");
        seed_virtual_mount(&state_root);
        {
            let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
            store
                .upsert_metadata_discovery_job(metadata_discovery_job(
                    "children:page-1",
                    MetadataDiscoveryPriority::Interactive,
                    3,
                ))
                .expect("queue discovery");
        }

        let runtime = runtime_state_for_root(state_root.clone());
        let requests = runtime.child_refreshes.debug_requests(10);

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].mount_id, "notion-main");
        assert_eq!(requests[0].container_identifier, "children:page-1");
        assert_eq!(requests[0].priority, ChildRefreshPriority::Interactive);
        assert_eq!(requests[0].depth, 3);

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn file_provider_children_queues_interactive_metadata_discovery() {
        let state_root = temp_runtime_root("runtime-file-provider-children-interactive-discovery");
        seed_virtual_mount(&state_root);
        let mut runtime = runtime_state_for_root(state_root.clone());
        runtime.active_job = Some(test_active_job());

        let (respond_to, response) = std::sync::mpsc::channel();
        runtime.handle_request(
            DaemonRequest::FileProviderChildren {
                mount_id: "notion-main".to_string(),
                container_identifier: "mount:notion-main".to_string(),
            },
            respond_to,
        );

        assert!(response.recv().expect("children response").ok);
        let requests = runtime.child_refreshes.debug_requests(10);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].container_identifier, "mount:notion-main");
        assert_eq!(requests[0].priority, ChildRefreshPriority::Interactive);

        let jobs = SqliteStateStore::open(state_root.clone())
            .expect("open store")
            .list_metadata_discovery_jobs()
            .expect("list discovery");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].container_identifier, "mount:notion-main");
        assert_eq!(jobs[0].priority, MetadataDiscoveryPriority::Interactive);

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn metadata_discovery_jobs_do_not_reload_when_background_sync_disabled() {
        let state_root = temp_runtime_root("runtime-metadata-discovery-background-disabled");
        seed_virtual_mount(&state_root);
        {
            let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
            store
                .upsert_metadata_discovery_job(metadata_discovery_job(
                    "children:page-1",
                    MetadataDiscoveryPriority::Interactive,
                    3,
                ))
                .expect("queue discovery");
        }

        let (sender, _receiver) = std::sync::mpsc::channel();
        let runtime = RuntimeState::new(
            crate::DaemonConfig {
                state_root: state_root.clone(),
                background_connector_sync: false,
                ..crate::DaemonConfig::default()
            },
            Arc::new(DefaultRuntimeJobRunner),
            sender,
        );

        assert!(runtime.child_refreshes.debug_requests(10).is_empty());
        assert_eq!(
            SqliteStateStore::open(state_root.clone())
                .expect("open store")
                .list_metadata_discovery_jobs()
                .expect("list discovery")
                .len(),
            1,
            "disable should suppress in-memory scheduling without deleting durable work"
        );

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn queue_child_refresh_persists_metadata_discovery_job() {
        let state_root = temp_runtime_root("runtime-metadata-discovery-persist");
        seed_virtual_mount(&state_root);
        let mut runtime = runtime_state_for_root(state_root.clone());
        runtime.active_job = Some(test_active_job());

        runtime.queue_child_refresh(
            "notion-main".to_string(),
            "children:page-1".to_string(),
            ChildRefreshPriority::Interactive,
            2,
        );

        let jobs = SqliteStateStore::open(state_root.clone())
            .expect("open store")
            .list_metadata_discovery_jobs()
            .expect("list discovery");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].container_identifier, "children:page-1");
        assert_eq!(jobs[0].priority, MetadataDiscoveryPriority::Interactive);
        assert_eq!(jobs[0].depth, 2);

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn successful_child_refresh_deletes_metadata_discovery_job() {
        let state_root = temp_runtime_root("runtime-metadata-discovery-success");
        seed_virtual_mount(&state_root);
        {
            let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
            store
                .upsert_metadata_discovery_job(metadata_discovery_job(
                    "children:page-1",
                    MetadataDiscoveryPriority::Background,
                    0,
                ))
                .expect("queue discovery");
        }
        let mut runtime = runtime_state_for_root(state_root.clone());

        runtime.handle_completion(JobCompletion::VirtualFsRefreshChildren {
            mount_id: "notion-main".to_string(),
            container_identifier: "children:page-1".to_string(),
            depth: 0,
            priority: ChildRefreshPriority::Background,
            result: Ok(VirtualFsRefreshChildrenReport::default()),
        });

        assert!(
            SqliteStateStore::open(state_root.clone())
                .expect("open store")
                .list_metadata_discovery_jobs()
                .expect("list discovery")
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn failed_child_refresh_records_metadata_discovery_failure() {
        let state_root = temp_runtime_root("runtime-metadata-discovery-failure");
        seed_virtual_mount(&state_root);
        {
            let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
            store
                .upsert_metadata_discovery_job(metadata_discovery_job(
                    "children:page-1",
                    MetadataDiscoveryPriority::Background,
                    0,
                ))
                .expect("queue discovery");
        }
        let mut runtime = runtime_state_for_root(state_root.clone());

        runtime.handle_completion(JobCompletion::VirtualFsRefreshChildren {
            mount_id: "notion-main".to_string(),
            container_identifier: "children:page-1".to_string(),
            depth: 0,
            priority: ChildRefreshPriority::Background,
            result: Err(LocalityError::InvalidState("rate limited".to_string())),
        });

        let jobs = SqliteStateStore::open(state_root.clone())
            .expect("open store")
            .list_metadata_discovery_jobs()
            .expect("list discovery");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].attempts, 1);
        assert_eq!(
            jobs[0].last_error.as_deref(),
            Some("invalid state: rate limited")
        );

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn failed_child_refresh_requeues_metadata_discovery_request() {
        let state_root = temp_runtime_root("runtime-metadata-discovery-failure-requeue");
        seed_virtual_mount(&state_root);
        {
            let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
            store
                .upsert_metadata_discovery_job(metadata_discovery_job(
                    "children:page-1",
                    MetadataDiscoveryPriority::Background,
                    0,
                ))
                .expect("queue discovery");
        }
        let mut runtime = runtime_state_for_root(state_root.clone());
        let request = ChildRefreshRequest {
            mount_id: "notion-main".to_string(),
            container_identifier: "children:page-1".to_string(),
            priority: ChildRefreshPriority::Background,
            depth: 0,
        };
        let popped = runtime
            .child_refreshes
            .pop_ready(&BTreeMap::new())
            .expect("loaded request is ready");
        assert_eq!(popped, request);
        runtime
            .active_child_refreshes
            .insert(request.key(), ActiveChildRefresh::new(request.clone()));
        runtime.active_job = Some(test_active_job());

        runtime.handle_completion(JobCompletion::VirtualFsRefreshChildren {
            mount_id: request.mount_id.clone(),
            container_identifier: request.container_identifier.clone(),
            depth: request.depth,
            priority: request.priority,
            result: Err(LocalityError::InvalidState("rate limited".to_string())),
        });

        assert_eq!(runtime.child_refreshes.debug_requests(10), vec![request]);

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn stale_child_refresh_target_completes_without_requeue_error() {
        let state_root = temp_runtime_root("runtime-stale-child-refresh-target");
        seed_virtual_mount(&state_root);

        let report = DefaultRuntimeJobRunner
            .run_virtual_fs_refresh_children(
                state_root.clone(),
                "notion-main".to_string(),
                "children:old-access-page".to_string(),
            )
            .expect("stale child refresh should be a no-op");

        assert_eq!(report, VirtualFsRefreshChildrenReport::default());

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn observable_remote_identifiers_exclude_virtual_only_items() {
        assert!(observable_remote_identifier(
            "3833ac0e-bb88-814b-b0e3-ea6963b6708a"
        ));
        assert!(!observable_remote_identifier("guidance:AGENTS.md"));
        assert!(!observable_remote_identifier("children:page-1"));
        assert!(!observable_remote_identifier("mount:notion-main"));
        assert!(!observable_remote_identifier("source:notion"));
    }

    #[test]
    fn virtual_fs_materialize_guidance_does_not_require_source_credentials() {
        let state_root = temp_runtime_root("runtime-guidance-materialize");
        let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
        store
            .save_connection(ConnectionRecord {
                connection_id: ConnectionId::new("notion-work"),
                profile_id: None,
                connector: "notion".to_string(),
                display_name: "Notion Work".to_string(),
                account_label: Some("work@example.com".to_string()),
                workspace_id: Some("workspace".to_string()),
                workspace_name: Some("Workspace".to_string()),
                auth_kind: "oauth".to_string(),
                secret_ref: "missing-secret".to_string(),
                scopes: Vec::new(),
                capabilities_json: "{}".to_string(),
                status: "active".to_string(),
                created_at: "2026-06-29T00:00:00Z".to_string(),
                updated_at: "2026-06-29T00:00:00Z".to_string(),
                expires_at: None,
            })
            .expect("save connection");
        store
            .save_mount(
                MountConfig::new(
                    MountId::new("notion-main"),
                    "notion",
                    state_root.join("Locality/notion-main"),
                )
                .with_connection_id(ConnectionId::new("notion-work"))
                .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");
        drop(store);

        let response = DefaultRuntimeJobRunner.run_virtual_fs_materialize(
            state_root.clone(),
            "notion-main".to_string(),
            "guidance:AGENTS.md".to_string(),
        );

        assert!(
            response.ok,
            "guidance materialization should stay local: {:?}",
            response.error
        );
        let payload = response.payload.expect("materialize payload");
        let report: crate::virtual_fs::VirtualFsMaterializeReport =
            serde_json::from_value(payload).expect("decode report");
        assert_eq!(report.identifier, "guidance:AGENTS.md");
        assert!(Path::new(&report.path).ends_with(Path::new(".loc-guidance").join("AGENTS.md")));
        let contents = std::fs::read_to_string(&report.path).expect("read guidance");
        assert!(contents.contains("Locality"));

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn virtual_fs_materialize_missing_guidance_mount_reports_mount_not_found() {
        let state_root = temp_runtime_root("runtime-guidance-missing-mount");
        SqliteStateStore::open(state_root.clone()).expect("open store");

        let response = DefaultRuntimeJobRunner.run_virtual_fs_materialize(
            state_root.clone(),
            "missing".to_string(),
            "guidance:AGENTS.md".to_string(),
        );

        assert!(!response.ok);
        assert_eq!(
            response.error.as_ref().map(|error| error.code.as_str()),
            Some("mount_not_found")
        );

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn file_provider_read_guidance_does_not_require_source_credentials() {
        use base64::Engine as _;

        let state_root = temp_runtime_root("runtime-guidance-read");
        let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
        store
            .save_connection(ConnectionRecord {
                connection_id: ConnectionId::new("notion-work"),
                profile_id: None,
                connector: "notion".to_string(),
                display_name: "Notion Work".to_string(),
                account_label: Some("work@example.com".to_string()),
                workspace_id: Some("workspace".to_string()),
                workspace_name: Some("Workspace".to_string()),
                auth_kind: "oauth".to_string(),
                secret_ref: "missing-secret".to_string(),
                scopes: Vec::new(),
                capabilities_json: "{}".to_string(),
                status: "active".to_string(),
                created_at: "2026-06-29T00:00:00Z".to_string(),
                updated_at: "2026-06-29T00:00:00Z".to_string(),
                expires_at: None,
            })
            .expect("save connection");
        store
            .save_mount(
                MountConfig::new(
                    MountId::new("notion-main"),
                    "notion",
                    state_root.join("Locality/notion-main"),
                )
                .with_connection_id(ConnectionId::new("notion-work"))
                .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");
        drop(store);

        let response = DefaultRuntimeJobRunner.run_file_provider_read(
            state_root.clone(),
            "notion-main".to_string(),
            "guidance:AGENTS.md".to_string(),
        );

        assert!(
            response.ok,
            "guidance read should stay local: {:?}",
            response.error
        );
        let payload = response.payload.expect("read payload");
        let report: crate::file_provider::FileProviderReadReport =
            serde_json::from_value(payload).expect("decode report");
        assert_eq!(report.identifier, "guidance:AGENTS.md");
        assert!(Path::new(&report.path).ends_with(Path::new(".loc-guidance").join("AGENTS.md")));
        assert_eq!(report.item.identifier, "guidance:AGENTS.md");
        assert_eq!(report.item.hydration, Some(HydrationState::Hydrated));
        let contents = base64::engine::general_purpose::STANDARD
            .decode(report.contents_base64)
            .expect("decode contents");
        let contents = String::from_utf8(contents).expect("utf8 guidance");
        assert!(contents.contains("Locality"));

        let _ = std::fs::remove_dir_all(state_root);
    }

    #[test]
    fn remote_fast_forward_hints_refresh_parent_children_when_child_links_change() {
        let previous = shadow_with_child_page_links("page-1", ["child-a"]);
        let current = shadow_with_child_page_links("page-1", ["child-a", "child-b"]);

        assert_eq!(
            remote_fast_forward_discovery_hints(&previous, &current),
            vec![RemoteDiscoveryHint::RefreshChildren {
                container_identifier: "children:page-1".to_string(),
            }]
        );
    }

    #[test]
    fn remote_fast_forward_hints_ignore_unchanged_child_links() {
        let previous = shadow_with_child_page_links("page-1", ["child-a"]);
        let current = shadow_with_child_page_links("page-1", ["child-a"]);

        assert!(remote_fast_forward_discovery_hints(&previous, &current).is_empty());
    }

    #[test]
    fn write_event_queues_auto_push_for_enrolled_dirty_file() {
        let fixture = RuntimeAutoSaveFixture::new();
        let mut store = fixture.store();
        fixture.write_page("Updated body.");

        let report = execute_file_event(
            &mut store,
            FileEvent {
                path: fixture.page_path.clone(),
                kind: FileEventKind::Write,
            },
        )
        .expect("file event");

        assert_eq!(report.report.marked_dirty, 1);
        assert_eq!(report.auto_push_targets, vec![fixture.page_path.clone()]);
    }

    #[test]
    fn write_event_queues_auto_push_for_mount_live_mode() {
        let fixture = RuntimeAutoSaveFixture::new();
        let mut store = fixture.store_with_mount_live_mode();
        fixture.write_page("Updated body.");

        let report = execute_file_event(
            &mut store,
            FileEvent {
                path: fixture.page_path.clone(),
                kind: FileEventKind::Write,
            },
        )
        .expect("file event");

        assert_eq!(report.report.marked_dirty, 1);
        assert_eq!(report.auto_push_targets, vec![fixture.page_path.clone()]);
    }

    #[test]
    fn remote_observation_pauses_auto_save_for_external_drift() {
        let fixture = RuntimeAutoSaveFixture::new();
        let mut store = fixture.store();
        let connector = ObservingConnector {
            observation: RemoteObservation {
                mount_id: fixture.mount_id.clone(),
                remote_id: fixture.remote_id.clone(),
                kind: EntityKind::Page,
                title: "Roadmap".to_string(),
                parent_remote_id: None,
                projected_path: "Roadmap.md".into(),
                remote_version: Some(RemoteVersion::new("remote-v2")),
                deleted: false,
                raw_metadata_json: "{}".to_string(),
            },
        };

        execute_observe_entity_job(
            &mut store,
            &connector,
            locality_core::freshness::SyncJob::new(
                fixture.mount_id.clone(),
                Some(fixture.remote_id.clone()),
                locality_core::freshness::SyncJobKind::ObserveEntity,
                locality_core::freshness::ChangeHintKind::RemoteMaybeChanged,
            ),
        )
        .expect("observe");

        let enrollment = store
            .get_auto_save_enrollment(&fixture.mount_id, "Roadmap.md".as_ref())
            .expect("get enrollment")
            .expect("enrollment");
        assert_eq!(enrollment.state, AutoSaveState::PausedRemoteChanged);
    }

    #[test]
    fn remote_observation_auto_deletes_unopened_online_only_remote_delete() {
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let mut store = InMemoryStateStore::new();
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(HydrationState::Stub)
                .with_remote_edited_at("remote-v1"),
            )
            .expect("entity");
        store
            .save_freshness_state(FreshnessStateRecord::new(
                mount_id.clone(),
                remote_id.clone(),
                FreshnessTier::Cold,
            ))
            .expect("freshness");
        let connector = ObservingConnector {
            observation: RemoteObservation {
                mount_id: mount_id.clone(),
                remote_id: remote_id.clone(),
                kind: EntityKind::Page,
                title: "Roadmap".to_string(),
                parent_remote_id: None,
                projected_path: "Roadmap.md".into(),
                remote_version: Some(RemoteVersion::new("remote-v1")),
                deleted: true,
                raw_metadata_json: "{}".to_string(),
            },
        };

        let report =
            execute_observe_entity_job(&mut store, &connector, observe_job(&mount_id, &remote_id))
                .expect("observe");

        assert!(!report.remote_hint_pending);
        assert!(
            store
                .get_entity(&mount_id, &remote_id)
                .expect("get entity")
                .is_none()
        );
        assert!(
            store
                .get_freshness_state(&mount_id, &remote_id)
                .expect("get freshness")
                .is_none()
        );
        assert!(
            store
                .get_remote_observation(&mount_id, &remote_id)
                .expect("get observation")
                .is_none()
        );
    }

    #[test]
    fn remote_observation_auto_deletes_clean_hydrated_plain_file_remote_delete() {
        let fixture = RuntimeAutoSaveFixture::new();
        let mut store = fixture.store_without_auto_save();
        let connector = ObservingConnector {
            observation: RemoteObservation {
                mount_id: fixture.mount_id.clone(),
                remote_id: fixture.remote_id.clone(),
                kind: EntityKind::Page,
                title: "Roadmap".to_string(),
                parent_remote_id: None,
                projected_path: "Roadmap.md".into(),
                remote_version: Some(RemoteVersion::new("remote-v1")),
                deleted: true,
                raw_metadata_json: "{}".to_string(),
            },
        };

        let report = execute_observe_entity_job(
            &mut store,
            &connector,
            observe_job(&fixture.mount_id, &fixture.remote_id),
        )
        .expect("observe");

        assert!(!report.remote_hint_pending);
        assert!(
            !fixture.page_path.exists(),
            "clean materialized page should be removed"
        );
        assert!(
            store
                .get_entity(&fixture.mount_id, &fixture.remote_id)
                .expect("get entity")
                .is_none()
        );
        assert!(
            store
                .get_freshness_state(&fixture.mount_id, &fixture.remote_id)
                .expect("get freshness")
                .is_none()
        );
        assert!(
            store
                .get_remote_observation(&fixture.mount_id, &fixture.remote_id)
                .expect("get observation")
                .is_none()
        );
    }

    #[test]
    fn repair_removes_stale_clean_hydrated_remote_delete_observation() {
        let fixture = RuntimeAutoSaveFixture::new();
        let mut store = fixture.store_without_auto_save();
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    fixture.mount_id.clone(),
                    fixture.remote_id.clone(),
                    FreshnessTier::Hot,
                )
                .remote_hint_pending(true),
            )
            .expect("save freshness");
        store
            .save_remote_observation(
                RemoteObservationRecord::new(
                    fixture.mount_id.clone(),
                    fixture.remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                    "unix_ms:1",
                )
                .with_remote_version(RemoteVersion::new("remote-v2"))
                .deleted(true),
            )
            .expect("save deleted observation");

        let report =
            repair_clean_remote_deleted_projections(&mut store, None, Some(&fixture.mount_id))
                .expect("repair remote delete");

        assert_eq!(report.projections_removed, 1);
        assert!(!fixture.page_path.exists());
        assert!(
            store
                .get_entity(&fixture.mount_id, &fixture.remote_id)
                .expect("get entity")
                .is_none()
        );
        assert!(
            store
                .get_remote_observation(&fixture.mount_id, &fixture.remote_id)
                .expect("get observation")
                .is_none()
        );
        assert!(
            store
                .get_freshness_state(&fixture.mount_id, &fixture.remote_id)
                .expect("get freshness")
                .is_none()
        );
    }

    #[test]
    fn remote_observation_keeps_dirty_hydrated_remote_delete_for_review() {
        let fixture = RuntimeAutoSaveFixture::new();
        let mut store = fixture.store_without_auto_save();
        fixture.write_page("Locally changed body.");
        let connector = ObservingConnector {
            observation: RemoteObservation {
                mount_id: fixture.mount_id.clone(),
                remote_id: fixture.remote_id.clone(),
                kind: EntityKind::Page,
                title: "Roadmap".to_string(),
                parent_remote_id: None,
                projected_path: "Roadmap.md".into(),
                remote_version: Some(RemoteVersion::new("remote-v1")),
                deleted: true,
                raw_metadata_json: "{}".to_string(),
            },
        };

        let report = execute_observe_entity_job(
            &mut store,
            &connector,
            observe_job(&fixture.mount_id, &fixture.remote_id),
        )
        .expect("observe");

        assert!(report.remote_hint_pending);
        assert!(fixture.page_path.exists(), "dirty page should be preserved");
        assert!(
            store
                .get_entity(&fixture.mount_id, &fixture.remote_id)
                .expect("get entity")
                .is_some()
        );
        assert!(
            store
                .get_freshness_state(&fixture.mount_id, &fixture.remote_id)
                .expect("get freshness")
                .expect("freshness")
                .remote_hint_pending
        );
        assert!(
            store
                .get_remote_observation(&fixture.mount_id, &fixture.remote_id)
                .expect("get observation")
                .expect("observation")
                .deleted
        );
    }

    #[test]
    fn remote_observation_keeps_opened_online_only_remote_delete_for_review() {
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let mut store = InMemoryStateStore::new();
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(HydrationState::Stub)
                .with_remote_edited_at("remote-v1"),
            )
            .expect("entity");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(mount_id.clone(), remote_id.clone(), FreshnessTier::Cold)
                    .opened_at("now"),
            )
            .expect("freshness");
        let connector = ObservingConnector {
            observation: RemoteObservation {
                mount_id: mount_id.clone(),
                remote_id: remote_id.clone(),
                kind: EntityKind::Page,
                title: "Roadmap".to_string(),
                parent_remote_id: None,
                projected_path: "Roadmap.md".into(),
                remote_version: Some(RemoteVersion::new("remote-v1")),
                deleted: true,
                raw_metadata_json: "{}".to_string(),
            },
        };

        let report =
            execute_observe_entity_job(&mut store, &connector, observe_job(&mount_id, &remote_id))
                .expect("observe");

        assert!(report.remote_hint_pending);
        assert!(
            store
                .get_entity(&mount_id, &remote_id)
                .expect("get entity")
                .is_some()
        );
        assert!(
            store
                .get_freshness_state(&mount_id, &remote_id)
                .expect("get freshness")
                .expect("freshness")
                .remote_hint_pending
        );
        assert!(
            store
                .get_remote_observation(&mount_id, &remote_id)
                .expect("get observation")
                .expect("observation")
                .deleted
        );
    }

    #[test]
    fn restored_remote_observation_clears_stale_deleted_hint_when_version_matches() {
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let mut store = InMemoryStateStore::new();
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(HydrationState::Hydrated)
                .with_remote_edited_at("remote-v1"),
            )
            .expect("entity");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(mount_id.clone(), remote_id.clone(), FreshnessTier::Warm)
                    .remote_hint_pending(true),
            )
            .expect("freshness");
        let connector = ObservingConnector {
            observation: RemoteObservation {
                mount_id: mount_id.clone(),
                remote_id: remote_id.clone(),
                kind: EntityKind::Page,
                title: "Roadmap".to_string(),
                parent_remote_id: None,
                projected_path: "Roadmap.md".into(),
                remote_version: Some(RemoteVersion::new("remote-v1")),
                deleted: false,
                raw_metadata_json: "{}".to_string(),
            },
        };

        let report =
            execute_observe_entity_job(&mut store, &connector, observe_job(&mount_id, &remote_id))
                .expect("observe");

        assert!(!report.remote_hint_pending);
        assert!(
            !store
                .get_freshness_state(&mount_id, &remote_id)
                .expect("get freshness")
                .expect("freshness")
                .remote_hint_pending
        );
        assert!(
            !store
                .get_remote_observation(&mount_id, &remote_id)
                .expect("get observation")
                .expect("observation")
                .deleted
        );
    }

    #[test]
    fn remote_observation_queues_live_mode_fast_forward_when_mount_live_mode_enabled() {
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let mut store = InMemoryStateStore::new();
        store
            .save_mount_live_mode(MountLiveModeRecord::new(
                mount_id.clone(),
                true,
                "unix_ms:1",
            ))
            .expect("save live mode");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(HydrationState::Hydrated)
                .with_remote_edited_at("remote-v1"),
            )
            .expect("entity");
        store
            .save_freshness_state(FreshnessStateRecord::new(
                mount_id.clone(),
                remote_id.clone(),
                FreshnessTier::Hot,
            ))
            .expect("freshness");
        let connector = ObservingConnector {
            observation: RemoteObservation {
                mount_id: mount_id.clone(),
                remote_id: remote_id.clone(),
                kind: EntityKind::Page,
                title: "Roadmap".to_string(),
                parent_remote_id: None,
                projected_path: "Roadmap.md".into(),
                remote_version: Some(RemoteVersion::new("remote-v2")),
                deleted: false,
                raw_metadata_json: "{}".to_string(),
            },
        };

        let report =
            execute_observe_entity_job(&mut store, &connector, observe_job(&mount_id, &remote_id))
                .expect("observe");

        assert_eq!(report.queued_hydrations.len(), 1);
        assert_eq!(
            report.queued_hydrations[0].reason,
            locality_core::hydration::HydrationReason::LiveModeRemoteFastForward
        );
    }

    fn request(
        mount_id: &str,
        container_identifier: &str,
        priority: ChildRefreshPriority,
        depth: u32,
    ) -> ChildRefreshRequest {
        ChildRefreshRequest {
            mount_id: mount_id.to_string(),
            container_identifier: container_identifier.to_string(),
            priority,
            depth,
        }
    }

    fn observe_job(mount_id: &MountId, remote_id: &RemoteId) -> SyncJob {
        SyncJob::new(
            mount_id.clone(),
            Some(remote_id.clone()),
            SyncJobKind::ObserveEntity,
            ChangeHintKind::RemoteMaybeChanged,
        )
    }

    struct RuntimeAutoSaveFixture {
        root: std::path::PathBuf,
        page_path: std::path::PathBuf,
        mount_id: MountId,
        remote_id: RemoteId,
    }

    impl RuntimeAutoSaveFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "loc-runtime-autosave-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&root).expect("root");
            let page_path = root.join("Roadmap.md");
            let fixture = Self {
                root,
                page_path,
                mount_id: MountId::new("notion-main"),
                remote_id: RemoteId::new("page-1"),
            };
            fixture.write_page("Original body.");
            fixture
        }

        fn store(&self) -> InMemoryStateStore {
            let mut store = self.store_without_auto_save();
            let mut enrollment = AutoSaveEnrollmentRecord::new(
                self.mount_id.clone(),
                "Roadmap.md",
                AutoSaveOrigin::LocalityCreated,
                "1",
            );
            enrollment.remote_id = Some(self.remote_id.clone());
            store
                .save_auto_save_enrollment(enrollment)
                .expect("enrollment");
            store
        }

        fn store_with_mount_live_mode(&self) -> InMemoryStateStore {
            let mut store = self.store_without_auto_save();
            store
                .save_mount_live_mode(MountLiveModeRecord::new(self.mount_id.clone(), true, "1"))
                .expect("live mode");
            store
        }

        fn store_without_auto_save(&self) -> InMemoryStateStore {
            let mut store = InMemoryStateStore::new();
            store
                .save_mount(MountConfig::new(
                    self.mount_id.clone(),
                    "notion",
                    self.root.clone(),
                ))
                .expect("mount");
            store
                .save_entity(
                    EntityRecord::new(
                        self.mount_id.clone(),
                        self.remote_id.clone(),
                        EntityKind::Page,
                        "Roadmap",
                        "Roadmap.md",
                    )
                    .with_hydration(HydrationState::Hydrated)
                    .with_remote_edited_at("remote-v1"),
                )
                .expect("entity");
            store
                .save_shadow(&self.mount_id, shadow("Original body."))
                .expect("shadow");
            store
        }

        fn write_page(&self, body: &str) {
            let document = CanonicalDocument::new(
                "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: remote-v1\ntitle: Roadmap\n",
                format!("# Roadmap\n\n{body}\n"),
            );
            std::fs::write(&self.page_path, render_canonical_markdown(&document))
                .expect("write page");
        }
    }

    impl Drop for RuntimeAutoSaveFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn shadow(body: &str) -> ShadowDocument {
        ShadowDocument::from_synced_body(
            RemoteId::new("page-1"),
            format!("# Roadmap\n\n{body}\n"),
            7,
            [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
        )
        .expect("shadow")
        .with_frontmatter("loc:\n  id: page-1\n  type: page\ntitle: Roadmap\n")
    }

    fn temp_runtime_root(prefix: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{unique}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn seed_virtual_mount(state_root: &Path) {
        let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open store");
        store
            .save_mount(
                MountConfig::new(
                    MountId::new("notion-main"),
                    "notion",
                    state_root.join("Locality/notion-main"),
                )
                .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");
    }

    fn runtime_state_for_root(state_root: PathBuf) -> RuntimeState {
        let (sender, _receiver) = std::sync::mpsc::channel();
        RuntimeState::new(
            crate::DaemonConfig {
                state_root,
                ..crate::DaemonConfig::default()
            },
            Arc::new(DefaultRuntimeJobRunner),
            sender,
        )
    }

    fn test_active_job() -> ActiveRuntimeJob {
        ActiveRuntimeJob {
            kind: "test".to_string(),
            target: None,
            started_at: std::time::Instant::now(),
            started_at_unix_ms: 0,
        }
    }

    fn metadata_discovery_job(
        container_identifier: &str,
        priority: MetadataDiscoveryPriority,
        depth: u32,
    ) -> MetadataDiscoveryJobRecord {
        MetadataDiscoveryJobRecord {
            mount_id: MountId::new("notion-main"),
            container_identifier: container_identifier.to_string(),
            priority,
            depth,
            attempts: 0,
            last_error: None,
            created_at: "2026-07-06T00:00:00Z".to_string(),
            updated_at: "2026-07-06T00:00:00Z".to_string(),
        }
    }

    fn shadow_with_child_page_links<const N: usize>(
        entity_id: &str,
        child_ids: [&str; N],
    ) -> ShadowDocument {
        let mut rendered_body = "# Parent\n".to_string();
        let mut blocks = vec![ShadowBlock {
            remote_id: RemoteId::new("heading-1"),
            kind: MarkdownBlockKind::Heading,
            source_span: SourceSpan {
                start_line: 1,
                end_line: 1,
            },
            content_hash: "heading".to_string(),
            text: "# Parent".to_string(),
            native_kind: Some("heading_1".to_string()),
        }];
        for (index, child_id) in child_ids.into_iter().enumerate() {
            let line = index + 3;
            rendered_body.push_str(&format!(
                "\n[Child {index}](https://www.notion.so/{child_id})\n"
            ));
            blocks.push(ShadowBlock {
                remote_id: RemoteId::new(child_id),
                kind: MarkdownBlockKind::Paragraph,
                source_span: SourceSpan {
                    start_line: line,
                    end_line: line,
                },
                content_hash: format!("child-{child_id}"),
                text: format!("[Child {index}](https://www.notion.so/{child_id})"),
                native_kind: Some("child_page".to_string()),
            });
        }
        ShadowDocument {
            entity_id: RemoteId::new(entity_id),
            frontmatter: String::new(),
            body_hash: format!("hash-{N}"),
            rendered_body,
            blocks,
        }
    }

    struct ObservingConnector {
        observation: RemoteObservation,
    }

    impl locality_connector::Connector for ObservingConnector {
        fn kind(&self) -> locality_connector::ConnectorKind {
            locality_connector::ConnectorKind("observing")
        }

        fn capabilities(&self) -> locality_connector::ConnectorCapabilities {
            locality_connector::ConnectorCapabilities::default()
        }

        fn observe(
            &self,
            _request: locality_connector::ObserveRequest,
        ) -> locality_core::LocalityResult<RemoteObservation> {
            Ok(self.observation.clone())
        }

        fn enumerate(
            &self,
            _request: locality_connector::EnumerateRequest,
        ) -> locality_core::LocalityResult<Vec<locality_core::model::TreeEntry>> {
            Err(locality_core::LocalityError::NotImplemented(
                "test connector",
            ))
        }

        fn fetch(
            &self,
            _request: locality_connector::FetchRequest,
        ) -> locality_core::LocalityResult<locality_connector::NativeEntity> {
            Err(locality_core::LocalityError::NotImplemented(
                "test connector",
            ))
        }

        fn render(
            &self,
            _entity: &locality_connector::NativeEntity,
        ) -> locality_core::LocalityResult<CanonicalDocument> {
            Err(locality_core::LocalityError::NotImplemented(
                "test connector",
            ))
        }

        fn parse(
            &self,
            _document: &CanonicalDocument,
        ) -> locality_core::LocalityResult<locality_connector::ParsedEntity> {
            Err(locality_core::LocalityError::NotImplemented(
                "test connector",
            ))
        }

        fn check_concurrency(
            &self,
            _request: locality_connector::ApplyPlanRequest<'_>,
        ) -> locality_core::LocalityResult<()> {
            Err(locality_core::LocalityError::NotImplemented(
                "test connector",
            ))
        }

        fn apply(
            &self,
            _request: locality_connector::ApplyPlanRequest<'_>,
        ) -> locality_core::LocalityResult<locality_connector::ApplyPlanResult> {
            Err(locality_core::LocalityError::NotImplemented(
                "test connector",
            ))
        }

        fn apply_undo(
            &self,
            _request: locality_connector::ApplyUndoRequest<'_>,
        ) -> locality_core::LocalityResult<locality_connector::ApplyUndoResult> {
            Err(locality_core::LocalityError::NotImplemented(
                "test connector",
            ))
        }
    }
}
