#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(clippy::items_after_test_module)]

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Condvar, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
#[cfg(target_os = "macos")]
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use loc_cli::connect::DEFAULT_NOTION_PROFILE_ID;
use loc_cli::connect::{
    BrokerOAuthConnectOptions, ConnectOptions, GmailBrokerOAuthConnectOptions,
    GoogleDocsBrokerOAuthConnectOptions, HttpGranolaConnectionProbe,
    run_connect_gmail_broker_oauth, run_connect_google_docs_broker_oauth, run_connect_granola,
    run_connect_notion_broker_oauth, run_disconnect,
};
use loc_cli::daemon::{DaemonRunState, run_daemon_control};
use loc_cli::diff::{DiffReport, run_diff};
#[cfg(target_os = "windows")]
use loc_cli::file_provider::{
    WindowsCloudFilesLifecycleAction, open_windows_cloud_files_sync_root,
    register_windows_cloud_files_sync_root, run_windows_cloud_files_lifecycle,
};
#[cfg(target_os = "macos")]
use loc_cli::file_provider::{
    macos_file_provider_domain_url, register_macos_file_provider_domain,
    run_macos_file_provider_helper,
};
use loc_cli::local_oauth::run_local_oauth_authorization;
use loc_cli::mount::{MountOptions, run_mount};
use loc_cli::pull::{PullReport, run_pull_with_state_root};
use loc_cli::push::{
    PushOptions, PushReport, push_report_exit_code, run_push_with_daemon_at_state_root,
};
use loc_cli::restore::{RestoreOptions, run_restore};
use loc_cli::search::{
    SearchOptions, SearchResult, is_notion_url_host, notion_id_from_url,
    run_search_with_access_roots, source_url_host,
};
use loc_cli::status::{StatusOptions, StatusState, StatusSyncState, run_status};
#[cfg(test)]
use locality_connector::ConnectorCapabilities;
use locality_connector::oauth_broker::OAuthBrokerStart;
use locality_core::canonical::parse_canonical_markdown;
use locality_core::conflict::{
    has_unresolved_conflict_markers, render_inline_conflict_markdown_with_base,
};
use locality_core::freshness::RemoteVersion;
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::journal::{JournalEntry, JournalStatus};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
use locality_gmail::{
    DEFAULT_GMAIL_OAUTH_BROKER_URL, DEFAULT_GMAIL_OAUTH_REDIRECT_URI, GMAIL_CONNECTOR_ID,
    HttpGmailOAuthBrokerClient,
};
use locality_google_docs::{
    DEFAULT_GOOGLE_DOCS_OAUTH_BROKER_URL, DEFAULT_GOOGLE_DOCS_OAUTH_REDIRECT_URI,
    GOOGLE_DOCS_CONNECTOR_ID, HttpGoogleDocsOAuthBrokerClient,
};
#[cfg(test)]
use locality_notion::NotionConfig;
#[cfg(test)]
use locality_notion::NotionConnector;
use locality_notion::client::notion_requests_per_second_setting;
#[cfg(test)]
use locality_notion::client::{HttpNotionApi, NotionApi};
#[cfg(test)]
use locality_notion::dto::{BlockDto, RichTextBlockDto, RichTextDto};
#[cfg(test)]
use locality_notion::oauth::StoredNotionCredential;
use locality_notion::oauth::{
    DEFAULT_LOCALITY_NOTION_OAUTH_BROKER_URL, HttpNotionOAuthBrokerClient, NotionOAuthBrokerStart,
};
use locality_platform::{
    DAEMON_PID_FILENAME, append_service_log, bundled_binary_next_to_current_exe,
    default_state_root as platform_default_state_root, logs_dir as platform_logs_dir,
    user_home as platform_user_home,
};
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, AutoSaveState, ConnectionId,
    ConnectionRecord, ConnectionRepository, EntityRecord, EntityRepository, FreshnessStateRecord,
    FreshnessStateRepository, HydrationJobRecord, HydrationJobRepository, JournalRepository,
    MountConfig, MountLiveModeRecord, MountLiveModeRepository, MountLiveModeState,
    MountLiveModeStateChangeError, MountRepository, ProjectionMode, RemoteObservationRecord,
    RemoteObservationRepository, ShadowRepository, SqliteStateStore, VirtualMutationKind,
    VirtualMutationRecord, VirtualMutationRepository, is_live_mode_state_change_signal_path,
    open_credential_store, reset_locality_state_storage, save_mount_live_mode_and_publish_signal,
};
#[cfg(test)]
use locality_store::{ConnectorProfileId, ConnectorProfileRecord, ConnectorProfileRepository};
use localityd::autosave::auto_save_timestamp;
use localityd::file_provider::{self as daemon_file_provider, ROOT_CONTAINER_IDENTIFIER};
use localityd::google_docs::resolve_google_docs_connector_for_mount;
use localityd::hydration::HydrationSource;
use localityd::ipc::{
    DaemonBuildInfo, DaemonDebugQueueStatus, DaemonRequest, DaemonStatusReport, send_request,
    send_request_with_timeout,
};
use localityd::media::{document_with_absolute_media_hrefs, update_hydrated_media_manifest};
use localityd::push::execute_auto_save_push_job_with_content_root;
use localityd::runtime::repair_clean_remote_deleted_projections;
use localityd::source::{
    ResolvedSource, SourceAdapter, resolve_source_for_mount_id, resolve_source_for_path,
    source_display_name,
};
#[cfg(target_os = "macos")]
use localityd::virtual_fs::materialize_virtual_fs_item_with_content_root;
#[cfg(target_os = "macos")]
use localityd::virtual_fs::mount_point_directory_name;
use localityd::virtual_fs::{
    VirtualFsChildrenReport, commit_virtual_fs_write, mount_point_identifier,
    virtual_fs_content_base, virtual_fs_content_path, virtual_fs_content_root,
};
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "macos")]
use tauri::TitleBarStyle;
use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, Position, Rect, WebviewUrl,
    WebviewWindowBuilder,
    image::Image,
    menu::{Menu, MenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::Color,
};
use tauri_plugin_dialog::DialogExt;

mod agent_guidance;

use agent_guidance::{
    AgentGuidanceInstallReport, install_agent_guidance as install_guidance_files,
    uninstall_agent_guidance as uninstall_guidance_files,
};

#[cfg(any(not(windows), test))]
const TERMINAL_CLI_PATH_MANAGED_START: &str = "# >>> LOCALITY_TERMINAL_CLI_PATH >>>";
#[cfg(any(not(windows), test))]
const TERMINAL_CLI_PATH_MANAGED_END: &str = "# <<< LOCALITY_TERMINAL_CLI_PATH <<<";
#[cfg(windows)]
const WINDOWS_TERMINAL_CLI_SHIM_MARKER: &str = "LOCALITY_TERMINAL_CLI_SHIM";
#[cfg(windows)]
const WINDOWS_RUN_KEY_PATH: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const DEFAULT_NOTION_MOUNT_POINT_DIRECTORY: &str = "notion";
#[cfg(windows)]
const WINDOWS_RUN_VALUE_NAME: &str = "Locality";
#[cfg(windows)]
const WINDOWS_DESKTOP_SINGLE_INSTANCE_MUTEX: &str =
    r"Local\CodeFlash.Locality.Desktop.SingleInstance";
#[cfg(windows)]
const WINDOWS_DESKTOP_ACTIVATION_EVENT: &str = r"Local\CodeFlash.Locality.Desktop.Activate";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const VIRTUAL_PROJECTION_SOURCE_READY_TIMEOUT: Duration = Duration::from_secs(30);
const VIRTUAL_PROJECTION_SOURCE_READY_POLL: Duration = Duration::from_millis(250);
const VIRTUAL_PROJECTION_SOURCE_READY_LOG_EVERY: Duration = Duration::from_secs(2);
#[cfg(target_os = "macos")]
const MACOS_FILE_PROVIDER_MOUNT_ROOT_APPEAR_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "macos")]
const MACOS_FILE_PROVIDER_MOUNT_ROOT_RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const LIVE_MODE_RUNNER_ACTIVE_INTERVAL: Duration = Duration::from_millis(500);
const LIVE_MODE_RUNNER_IDLE_RECHECK: Duration = Duration::from_secs(5 * 60);
const LIVE_MODE_RUNNER_PERIODIC_RECHECK: Duration = LIVE_MODE_ACTIVE_REMOTE_CHECK_INTERVAL;
const DESKTOP_BACKGROUND_LAUNCH_ARG: &str = "--background";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopSnapshot {
    health: AppHealth,
    connection: ConnectionSummary,
    connections: Vec<ConnectionSummary>,
    mount: MountSummary,
    mounts: Vec<MountSummary>,
    active_mount_id: Option<String>,
    live_mode: MountLiveModeSummary,
    needs_onboarding: bool,
    settings: DesktopSettings,
    pending_changes: Vec<PendingChange>,
    recent_files: Vec<LocatedItem>,
    activity: Vec<ActivityItem>,
    suggestions: Vec<ConnectorSuggestion>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppHealth {
    state: String,
    attention_count: usize,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionSummary {
    connector: String,
    workspace_name: String,
    account_label: String,
    status: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MountSummary {
    mount_id: String,
    connector: String,
    connector_name: String,
    connection_id: Option<String>,
    workspace_name: String,
    local_path: String,
    notion_url: Option<String>,
    access_scope: String,
    remote_root_id: Option<String>,
    projection: String,
    read_only: bool,
    status: String,
    root_exists: bool,
    entity_count: usize,
    pending_change_count: usize,
    provider: Option<ProviderRuntimeSummary>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderRuntimeSummary {
    state: String,
    message: String,
    daemon_running: bool,
    registered: Option<bool>,
    pid: Option<u32>,
    stale_pid_file: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DesktopSettings {
    launch_at_login: bool,
    show_menu_bar: bool,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            launch_at_login: true,
            show_menu_bar: true,
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PendingChange {
    mount_id: String,
    entity_id: String,
    title: String,
    local_path: String,
    summary: String,
    state: String,
    issue_codes: Vec<String>,
    live_mode: LiveModeFileStatus,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveModeFileStatus {
    enabled: bool,
    state: String,
    label: String,
    reason: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MountLiveModeSummary {
    enabled: bool,
    state: String,
    label: String,
    reason: Option<String>,
    last_run_at: Option<String>,
    pending_count: usize,
    review_count: usize,
    covered_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityItem {
    title: String,
    detail: String,
    when: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    occurred_at: Option<String>,
    kind: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorSuggestion {
    connector: String,
    description: String,
    state: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LocatedItem {
    title: String,
    kind: String,
    local_path: String,
    state: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PushPlan {
    title: String,
    summary: String,
    pages_updated: usize,
    database_rows_updated: usize,
    pages_deleted: usize,
    can_push: bool,
    guardrail_state: String,
    files: Vec<PendingChange>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ActionReport {
    ok: bool,
    message: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceMountOnboardingReport {
    state: String,
    message: String,
    primary_action: String,
    launch_strategy: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileProviderEnablementReport {
    state: String,
    message: String,
    path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MacosWorkspaceMountOnboardingState {
    Created,
    ApprovalRequired,
    WaitingForCloudStorageRoot,
    Failed,
}

impl MacosWorkspaceMountOnboardingState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::ApprovalRequired => "needs_finder_enable",
            Self::WaitingForCloudStorageRoot => "waiting_for_cloudstorage_root",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceMountOnboardingPrimaryAction {
    AllowInMacos,
    CheckAgain,
    RetrySetup,
}

impl WorkspaceMountOnboardingPrimaryAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::AllowInMacos => "allow_in_macos",
            Self::CheckAgain => "check_again",
            Self::RetrySetup => "retry_setup",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceMountOnboardingLaunchStrategy {
    OpenFinder,
    InstructionsOnly,
    None,
}

impl WorkspaceMountOnboardingLaunchStrategy {
    fn as_str(self) -> &'static str {
        match self {
            Self::OpenFinder => "open_finder",
            Self::InstructionsOnly => "instructions_only",
            Self::None => "none",
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MountIdRequest {
    mount_id: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateDesktopMountRequest {
    connector: String,
    path: String,
    mount_id: String,
    connection_id: Option<String>,
    read_only: bool,
    notion_root_page: Option<String>,
    google_docs_workspace_folder: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceMountOnboardingRequest {
    path: String,
    action: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceMountOnboardingAction {
    Start,
    AllowInMacos,
    CheckAgain,
}

impl WorkspaceMountOnboardingAction {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "start" => Ok(Self::Start),
            "allow_in_macos" => Ok(Self::AllowInMacos),
            "check_again" => Ok(Self::CheckAgain),
            other => Err(format!("Unsupported onboarding mount action `{other}`.")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LiveModeRemoteTarget {
    mount_id: MountId,
    remote_id: RemoteId,
    path: PathBuf,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileDetailReport {
    ok: bool,
    path: String,
    has_conflict_markers: bool,
    conflict_preview: Option<String>,
    message: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileEditorReport {
    ok: bool,
    path: String,
    contents: String,
    has_conflict_markers: bool,
    message: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallStateReview {
    should_prompt: bool,
    state_exists: bool,
    sqlite_exists: bool,
    previous_build_id: Option<String>,
    current_build_id: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DesktopInstallMarker {
    #[serde(default)]
    state_format_version: u32,
    app_version: String,
    #[serde(default)]
    app_build_id: String,
    daemon_build_id: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DesktopSettingChange {
    key: String,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LiveModeFileChange {
    path: String,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MountLiveModeChange {
    enabled: bool,
}

static CONNECT_NOTION_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static CONNECT_GOOGLE_DOCS_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static CONNECT_GMAIL_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static DAEMON_LIFECYCLE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static NOTION_LOGIN_LINK: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static DESKTOP_SNAPSHOT_CACHE: OnceLock<Mutex<DesktopSnapshotCache>> = OnceLock::new();
static SURFACE_REFRESH_STATE: OnceLock<Mutex<SurfaceRefreshState>> = OnceLock::new();
static LAUNCH_AT_LOGIN_STATE: OnceLock<Mutex<Option<bool>>> = OnceLock::new();
static LIVE_MODE_REMOTE_PULL_CURSOR: OnceLock<Mutex<usize>> = OnceLock::new();
static LIVE_MODE_REMOTE_PULL_SCAN_TIMES: OnceLock<Mutex<BTreeMap<MountId, Instant>>> =
    OnceLock::new();
static LIVE_MODE_REMOTE_FAST_FORWARD_TIMES: OnceLock<
    Mutex<BTreeMap<(MountId, RemoteId), Instant>>,
> = OnceLock::new();
static LIVE_MODE_LOCAL_RECONCILE_TIMES: OnceLock<Mutex<BTreeMap<PathBuf, Instant>>> =
    OnceLock::new();
static LIVE_MODE_TICK_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static LOCAL_STATE_RESET_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "windows")]
static WINDOWS_CLOUD_FILES_PROVIDER_SUPERVISOR: OnceLock<
    Mutex<WindowsCloudFilesProviderSupervisor>,
> = OnceLock::new();
const DESKTOP_INSTALL_MARKER_VERSION: u32 = 2;
const DESKTOP_ACTIVITY_LIMIT: usize = 20;
const DESKTOP_SNAPSHOT_CACHE_TTL: Duration = Duration::from_millis(750);
const SURFACE_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(2);
const TRAY_POPOVER_WIDTH: f64 = 360.0;
const TRAY_POPOVER_HEIGHT: f64 = 520.0;
const TRAY_POPOVER_EDGE_MARGIN: f64 = 8.0;
const TRAY_POPOVER_ANCHOR_OFFSET: f64 = 12.0;
const LIVE_MODE_LOCAL_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
const LIVE_MODE_ACTIVE_REMOTE_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const LIVE_MODE_REMOTE_FAST_FORWARD_LEASE: Duration = Duration::from_secs(30);
const LIVE_MODE_ACTIVE_TARGET_WINDOW: Duration = Duration::from_secs(5 * 60);
const LIVE_MODE_REMOTE_CHECK_ESTIMATED_REQUESTS_PER_PAGE: f64 = 1.0;
const LIVE_MODE_REMOTE_CHECK_QUEUE_SHARE: f64 = 1.0 / 3.0;
const LIVE_MODE_REMOTE_CHECK_MAX_BATCH_PAGES: usize = 5;

#[derive(Default)]
struct DesktopSnapshotCache {
    loaded_at: Option<Instant>,
    snapshot: Option<DesktopSnapshot>,
}

#[derive(Default)]
struct SurfaceRefreshState {
    last_requested: Option<Instant>,
    scheduled: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrayVisualState {
    Ready,
    Review,
    Reconnect,
}

#[derive(Clone, PartialEq, Eq)]
struct TrayRenderState {
    state: TrayVisualState,
    tooltip: String,
}

static LAST_TRAY_RENDER: OnceLock<Mutex<Option<TrayRenderState>>> = OnceLock::new();

struct DesktopSingleInstanceGuard {
    #[cfg(windows)]
    mutex_handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(windows)]
    activation_event_handle: windows_sys::Win32::Foundation::HANDLE,
}

impl DesktopSingleInstanceGuard {
    fn activation_event_handle(&self) -> Option<usize> {
        #[cfg(windows)]
        {
            if self.activation_event_handle.is_null() {
                None
            } else {
                Some(self.activation_event_handle as usize)
            }
        }

        #[cfg(not(windows))]
        {
            None
        }
    }
}

#[cfg(windows)]
impl Drop for DesktopSingleInstanceGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        unsafe {
            if !self.activation_event_handle.is_null() {
                let _ = CloseHandle(self.activation_event_handle);
            }
            if !self.mutex_handle.is_null() {
                let _ = CloseHandle(self.mutex_handle);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScreenBounds {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl ScreenBounds {
    fn contains(&self, point: PhysicalPosition<f64>) -> bool {
        point.x >= self.left
            && point.x <= self.right
            && point.y >= self.top
            && point.y <= self.bottom
    }

    fn distance_squared_to(&self, point: PhysicalPosition<f64>) -> f64 {
        let dx = if point.x < self.left {
            self.left - point.x
        } else if point.x > self.right {
            point.x - self.right
        } else {
            0.0
        };
        let dy = if point.y < self.top {
            self.top - point.y
        } else if point.y > self.bottom {
            point.y - self.bottom
        } else {
            0.0
        };
        (dx * dx) + (dy * dy)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct MonitorScreenBounds {
    screen: ScreenBounds,
    work_area: ScreenBounds,
}

#[tauri::command]
async fn desktop_snapshot(app: AppHandle) -> DesktopSnapshot {
    let snapshot = match tauri::async_runtime::spawn_blocking(load_desktop_snapshot_for_surface)
        .await
    {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(message)) => degraded_snapshot(message),
        Err(error) => degraded_snapshot(format!("Could not load Locality desktop state: {error}")),
    };
    refresh_tray_icon_for_snapshot(&app, &snapshot);
    snapshot
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopDebugQueueStatus {
    #[serde(flatten)]
    queue: DaemonDebugQueueStatus,
    live_mode: DesktopLiveModeDebugStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopLiveModeDebugStatus {
    mount_id: Option<String>,
    enabled: bool,
    state: String,
    label: String,
    reason: Option<String>,
    last_run_at: Option<String>,
    tracked_files: Vec<DesktopLiveModeDebugFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopLiveModeDebugFile {
    path: String,
    title: String,
    remote_id: String,
    hydration: String,
    status: String,
    sync_state: String,
    active_for_polling: bool,
    remote_check_due: bool,
    polling_reason: Option<String>,
    freshness_tier: Option<String>,
    last_checked_at: Option<String>,
    last_opened_at: Option<String>,
    last_local_change_at: Option<String>,
    remote_hint_pending: bool,
    auto_save_state: Option<String>,
    auto_save_reason: Option<String>,
    issue_codes: Vec<String>,
}

#[tauri::command]
async fn debug_notion_queue_status() -> Result<DesktopDebugQueueStatus, String> {
    tauri::async_runtime::spawn_blocking(debug_notion_queue_status_blocking)
        .await
        .map_err(|error| format!("Could not inspect daemon debug queue: {error}"))?
}

fn debug_notion_queue_status_blocking() -> Result<DesktopDebugQueueStatus, String> {
    let response = send_request_with_timeout(
        &default_state_root(),
        &DaemonRequest::DebugQueueStatus,
        Duration::from_secs(1),
    )
    .map_err(|error| format!("Daemon debug queue is unavailable: {}", error.message()))?;
    if !response.ok {
        return Err(response
            .error
            .map(|error| error.message)
            .unwrap_or_else(|| "Daemon debug queue returned an unknown error.".to_string()));
    }
    let payload = response
        .payload
        .ok_or_else(|| "Daemon debug queue returned an empty response.".to_string())?;
    let queue = serde_json::from_value(payload)
        .map_err(|error| format!("Could not decode daemon debug queue: {error}"))?;
    let live_mode = debug_live_mode_status_blocking()?;
    Ok(DesktopDebugQueueStatus { queue, live_mode })
}

fn debug_live_mode_status_blocking() -> Result<DesktopLiveModeDebugStatus, String> {
    let state_root = default_state_root();
    let store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state for Live Mode debug: {error}"))?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Could not load mounts for Live Mode debug: {error}"))?;
    let connections = store
        .list_connections()
        .map_err(|error| format!("Could not load connections for Live Mode debug: {error}"))?;
    let Some(mount) = choose_mount(&mounts, &connections) else {
        return Ok(DesktopLiveModeDebugStatus {
            mount_id: None,
            enabled: false,
            state: "off".to_string(),
            label: "Live Mode off".to_string(),
            reason: None,
            last_run_at: None,
            tracked_files: Vec::new(),
        });
    };
    let pending_changes = pending_changes_for_mount(&store, &state_root, &mount.mount_id)?;
    let summary = mount_live_mode_summary(&store, Some(&mount), &pending_changes);
    let tracked_files =
        debug_live_mode_tracked_files_for_mount(&store, &state_root, &mount, &pending_changes)?;

    Ok(DesktopLiveModeDebugStatus {
        mount_id: Some(mount.mount_id.0),
        enabled: summary.enabled,
        state: summary.state,
        label: summary.label,
        reason: summary.reason,
        last_run_at: summary.last_run_at,
        tracked_files,
    })
}

fn debug_live_mode_tracked_files_for_mount(
    store: &SqliteStateStore,
    state_root: &Path,
    mount: &MountConfig,
    pending_changes: &[PendingChange],
) -> Result<Vec<DesktopLiveModeDebugFile>, String> {
    let freshness_by_remote_id = store
        .list_freshness_states(&mount.mount_id)
        .map_err(|error| format!("Could not inspect Live Mode freshness state: {error}"))?
        .into_iter()
        .map(|freshness| (freshness.remote_id.clone(), freshness))
        .collect::<BTreeMap<_, _>>();
    let autosave_by_path = store
        .list_auto_save_enrollments(&mount.mount_id)
        .map_err(|error| format!("Could not inspect Live Mode file state: {error}"))?
        .into_iter()
        .map(|enrollment| (enrollment.path.clone(), enrollment))
        .collect::<BTreeMap<_, _>>();
    let mut paths = pending_changes
        .iter()
        .map(|change| PathBuf::from(&change.local_path))
        .collect::<BTreeSet<_>>();
    for enrollment in autosave_by_path.values() {
        paths.insert(enrollment.path.clone());
    }
    for entity in store
        .list_entities(&mount.mount_id)
        .map_err(|error| format!("Could not inspect mounted files for Live Mode debug: {error}"))?
    {
        if entity.kind != EntityKind::Page {
            continue;
        }
        if entity.hydration == HydrationState::Dirty
            || entity.hydration == HydrationState::Conflicted
        {
            paths.insert(entity.path.clone());
            continue;
        }
        if let Some(freshness) = freshness_by_remote_id.get(&entity.remote_id)
            && (freshness.remote_hint_pending
                || live_mode_target_is_recently_active(Some(freshness), live_mode_now_ms()))
        {
            paths.insert(entity.path.clone());
        }
    }

    let access_root = mount_access_root(mount);
    let mut files = Vec::new();
    for path in paths.into_iter().take(120) {
        let absolute = access_root.join(&path);
        let status = match run_status(
            store,
            StatusOptions {
                path: Some(absolute),
                state_root: Some(state_root.to_path_buf()),
                ..StatusOptions::default()
            },
        ) {
            Ok(status) => status,
            Err(_) => continue,
        };
        let Some(entry) = status
            .mounts
            .iter()
            .find(|mount_status| mount_status.mount_id == mount.mount_id.0)
            .and_then(|mount_status| mount_status.entries.first())
        else {
            continue;
        };
        let remote_id = RemoteId::new(entry.entity_id.clone());
        let freshness = freshness_by_remote_id.get(&remote_id);
        let enrollment = autosave_by_path.get(Path::new(&entry.path));
        let now_ms = live_mode_now_ms();
        let active_for_polling = live_mode_target_is_recently_active(freshness, now_ms);
        let remote_check_due =
            active_for_polling && live_mode_remote_check_is_due(freshness, now_ms);
        files.push(DesktopLiveModeDebugFile {
            path: entry.path.clone(),
            title: entry.title.clone(),
            remote_id: entry.entity_id.clone(),
            hydration: format!("{:?}", entry.state).to_ascii_lowercase(),
            status: pending_state_for_entry(entry).to_string(),
            sync_state: format!("{:?}", entry.sync_state).to_ascii_lowercase(),
            active_for_polling,
            remote_check_due,
            polling_reason: debug_live_mode_polling_reason(freshness, now_ms),
            freshness_tier: freshness
                .map(|freshness| format!("{:?}", freshness.tier).to_ascii_lowercase()),
            last_checked_at: freshness.and_then(|freshness| freshness.last_checked_at.clone()),
            last_opened_at: freshness.and_then(|freshness| freshness.last_opened_at.clone()),
            last_local_change_at: freshness
                .and_then(|freshness| freshness.last_local_change_at.clone()),
            remote_hint_pending: freshness.is_some_and(|freshness| freshness.remote_hint_pending),
            auto_save_state: enrollment
                .map(|enrollment| format!("{:?}", enrollment.state).to_ascii_lowercase()),
            auto_save_reason: enrollment.and_then(|enrollment| enrollment.last_reason.clone()),
            issue_codes: entry
                .issues
                .iter()
                .map(|issue| issue.code.clone())
                .collect(),
        });
    }
    files.sort_by(|left, right| {
        debug_live_mode_file_rank(left)
            .cmp(&debug_live_mode_file_rank(right))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(files)
}

fn debug_live_mode_polling_reason(
    freshness: Option<&FreshnessStateRecord>,
    now_ms: u128,
) -> Option<String> {
    let freshness = freshness?;
    if freshness.remote_hint_pending {
        return Some("remote hint pending".to_string());
    }
    if live_mode_timestamp_is_within(
        freshness.last_local_change_at.as_deref(),
        now_ms,
        LIVE_MODE_ACTIVE_TARGET_WINDOW,
    ) {
        return Some("recent local edit".to_string());
    }
    if live_mode_timestamp_is_within(
        freshness.last_opened_at.as_deref(),
        now_ms,
        LIVE_MODE_ACTIVE_TARGET_WINDOW,
    ) {
        return Some("recently opened".to_string());
    }
    None
}

fn debug_live_mode_file_rank(file: &DesktopLiveModeDebugFile) -> u8 {
    match file.status.as_str() {
        "conflict" => 0,
        "needs_review" | "pending_changes" => 1,
        "remote_update_available" => 2,
        _ if file.remote_hint_pending => 3,
        _ => 4,
    }
}

#[tauri::command]
async fn connect_notion(app: AppHandle) -> ActionReport {
    run_notion_connection_flow(app, NotionConnectionAction::Connect, true).await
}

#[tauri::command]
async fn connect_notion_without_browser(app: AppHandle) -> ActionReport {
    run_notion_connection_flow(app, NotionConnectionAction::Connect, false).await
}

#[tauri::command]
async fn change_notion_access(app: AppHandle) -> ActionReport {
    run_notion_connection_flow(app, NotionConnectionAction::ChangeAccess, true).await
}

#[tauri::command]
async fn connect_google_docs(app: AppHandle) -> ActionReport {
    run_google_docs_connection_flow(app, true).await
}

#[tauri::command]
async fn connect_gmail(app: AppHandle) -> ActionReport {
    run_gmail_connection_flow(app, true).await
}

#[derive(Clone, Copy)]
enum NotionConnectionAction {
    Connect,
    ChangeAccess,
}

impl NotionConnectionAction {
    fn activity_title(self) -> &'static str {
        match self {
            Self::Connect => "Connected Notion workspace",
            Self::ChangeAccess => "Changed Notion access",
        }
    }

    fn activity_kind(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::ChangeAccess => "access",
        }
    }

    fn failure_label(self) -> &'static str {
        match self {
            Self::Connect => "connect notion",
            Self::ChangeAccess => "change notion access",
        }
    }
}

async fn run_notion_connection_flow(
    app: AppHandle,
    action: NotionConnectionAction,
    open_browser: bool,
) -> ActionReport {
    if CONNECT_NOTION_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return ActionReport {
            ok: false,
            message: "A Notion connection flow is already waiting for browser approval."
                .to_string(),
        };
    }
    clear_notion_login_link();

    let state_root = default_state_root();
    let activity_state_root = state_root.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        connect_notion_with_broker(state_root, open_browser)
    })
    .await
    .map_err(|error| format!("Notion OAuth worker failed: {error}"));
    CONNECT_NOTION_IN_PROGRESS.store(false, Ordering::Release);
    clear_notion_login_link();

    let report = match result {
        Ok(Ok(message)) => {
            if let Err(error) = record_desktop_activity(
                &activity_state_root,
                action.activity_title(),
                &message,
                action.activity_kind(),
            ) {
                desktop_log(
                    "warn",
                    "activity.record_failed",
                    format!("could not record Notion access activity: {error}"),
                );
            }
            ActionReport { ok: true, message }
        }
        Ok(Err(message)) | Err(message) => {
            desktop_log(
                "warn",
                "notion_access.failed",
                format!("{} failed: {message}", action.failure_label()),
            );
            ActionReport { ok: false, message }
        }
    };
    if report.ok {
        refresh_desktop_surfaces(&app);
    }
    report
}

async fn run_google_docs_connection_flow(app: AppHandle, open_browser: bool) -> ActionReport {
    if CONNECT_GOOGLE_DOCS_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return ActionReport {
            ok: false,
            message: "A Google Docs connection flow is already waiting for browser approval."
                .to_string(),
        };
    }

    let state_root = default_state_root();
    let activity_state_root = state_root.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        connect_google_docs_with_broker(state_root, open_browser)
    })
    .await
    .map_err(|error| format!("Google Docs OAuth worker failed: {error}"));
    CONNECT_GOOGLE_DOCS_IN_PROGRESS.store(false, Ordering::Release);

    let report = match result {
        Ok(Ok(message)) => {
            if let Err(error) = record_desktop_activity(
                &activity_state_root,
                "Connected Google Docs",
                &message,
                "connect",
            ) {
                desktop_log(
                    "warn",
                    "activity.record_failed",
                    format!("could not record Google Docs access activity: {error}"),
                );
            }
            ActionReport { ok: true, message }
        }
        Ok(Err(message)) | Err(message) => {
            desktop_log(
                "warn",
                "google_docs_access.failed",
                format!("connect google docs failed: {message}"),
            );
            ActionReport { ok: false, message }
        }
    };
    if report.ok {
        refresh_desktop_surfaces(&app);
    }
    report
}

async fn run_gmail_connection_flow(app: AppHandle, open_browser: bool) -> ActionReport {
    if CONNECT_GMAIL_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return ActionReport {
            ok: false,
            message: "A Gmail connection flow is already waiting for browser approval.".to_string(),
        };
    }

    let state_root = default_state_root();
    let activity_state_root = state_root.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        connect_gmail_with_broker(state_root, open_browser)
    })
    .await
    .map_err(|error| format!("Gmail OAuth worker failed: {error}"));
    CONNECT_GMAIL_IN_PROGRESS.store(false, Ordering::Release);

    let report = match result {
        Ok(Ok(message)) => {
            if let Err(error) = record_desktop_activity(
                &activity_state_root,
                "Connected Gmail",
                &message,
                "connect",
            ) {
                desktop_log(
                    "warn",
                    "activity.record_failed",
                    format!("could not record Gmail access activity: {error}"),
                );
            }
            ActionReport { ok: true, message }
        }
        Ok(Err(message)) | Err(message) => {
            desktop_log(
                "warn",
                "gmail_access.failed",
                format!("connect gmail failed: {message}"),
            );
            ActionReport { ok: false, message }
        }
    };
    if report.ok {
        refresh_desktop_surfaces(&app);
    }
    report
}

#[tauri::command]
fn notion_login_link() -> Option<String> {
    notion_login_link_slot()
        .lock()
        .ok()
        .and_then(|link| link.clone())
}

#[tauri::command]
async fn install_state_review() -> InstallStateReview {
    tauri::async_runtime::spawn_blocking(|| inspect_install_state(&default_state_root()))
        .await
        .unwrap_or_else(|_| inspect_install_state(&default_state_root()))
}

#[tauri::command]
async fn acknowledge_install_state() -> ActionReport {
    match tauri::async_runtime::spawn_blocking(|| {
        acknowledge_install_state_at(&default_state_root())
    })
    .await
    .map_err(|error| format!("Install marker worker failed: {error}"))
    .and_then(|result| result)
    {
        Ok(report) => report,
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
async fn reset_locality_state(app: AppHandle) -> ActionReport {
    match tauri::async_runtime::spawn_blocking(|| {
        let state_root = default_state_root();
        reset_locality_state_at(&state_root)
            .and_then(|_| record_current_install_marker(&state_root))
    })
    .await
    .map_err(|error| format!("Reset worker failed: {error}"))
    .and_then(|result| result)
    {
        Ok(()) => {
            refresh_desktop_surfaces(&app);
            ActionReport {
                ok: true,
                message: "Locality local state was reset. Local files were left in place."
                    .to_string(),
            }
        }
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
async fn prepare_locality_uninstall(app: AppHandle) -> ActionReport {
    match tauri::async_runtime::spawn_blocking(|| {
        let state_root = default_state_root();
        prepare_locality_uninstall_at(&state_root)
    })
    .await
    .map_err(|error| format!("Uninstall preparation worker failed: {error}"))
    .and_then(|result| result)
    {
        Ok(()) => {
            refresh_desktop_surfaces(&app);
            ActionReport {
                ok: true,
                message: "Locality is ready to uninstall. The app will quit; you can move Locality.app to the Trash."
                    .to_string(),
            }
        }
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
async fn choose_mount_folder(
    app: AppHandle,
    current: Option<String>,
) -> Result<Option<String>, String> {
    let selected = choose_folder_with_dialog(&app, current)?;
    selected
        .map(|path| {
            let root = normalize_desktop_mount_root(&path)?;
            validate_desktop_mount_root(&root, &default_state_root(), &desktop_projection_mode())?;
            Ok(absolute_display_path(&root))
        })
        .transpose()
}

#[tauri::command]
async fn ensure_runtime_ready(app: AppHandle) -> ActionReport {
    match tauri::async_runtime::spawn_blocking(|| {
        let state_root = default_state_root();
        ensure_virtual_projection_domains_for_state(&state_root)
            .and_then(|_| ensure_daemon_running(&state_root))
            .and_then(|_| reload_daemon_mounts(&state_root))
            .and_then(|_| ensure_virtual_projection_runtimes_for_state(&state_root))
    })
    .await
    .map_err(|error| format!("Runtime worker failed: {error}"))
    .and_then(|result| result)
    {
        Ok(()) => {
            refresh_desktop_surfaces(&app);
            ActionReport {
                ok: true,
                message: "Locality runtime is running.".to_string(),
            }
        }
        Err(message) => ActionReport { ok: false, message },
    }
}

fn ensure_runtime_ready_in_background(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let report = ensure_runtime_ready(app).await;
        if !report.ok {
            desktop_log(
                "warn",
                "runtime.startup_failed",
                format!(
                    "could not prepare Locality runtime after desktop startup: {}",
                    report.message
                ),
            );
        }
    });
}

#[tauri::command]
async fn ensure_terminal_cli_available() -> ActionReport {
    match tauri::async_runtime::spawn_blocking(install_terminal_cli_link)
        .await
        .map_err(|error| format!("Terminal CLI worker failed: {error}"))
        .and_then(|result| result)
    {
        Ok(path) => ActionReport {
            ok: true,
            message: format!("Locality terminal command is ready at {}.", path.display()),
        },
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
async fn create_workspace_mount(app: AppHandle, path: String) -> ActionReport {
    create_desktop_mount_command(
        app,
        CreateDesktopMountRequest {
            connector: "notion".to_string(),
            path,
            mount_id: "notion-main".to_string(),
            connection_id: None,
            read_only: false,
            notion_root_page: None,
            google_docs_workspace_folder: None,
        },
    )
    .await
}

#[tauri::command]
async fn create_desktop_mount(app: AppHandle, request: CreateDesktopMountRequest) -> ActionReport {
    create_desktop_mount_command(app, request).await
}

#[tauri::command]
async fn connect_granola(app: AppHandle, api_key: String) -> ActionReport {
    let report = tauri::async_runtime::spawn_blocking(move || connect_granola_blocking(api_key))
        .await
        .map_err(|error| format!("Granola connection worker failed: {error}"))
        .and_then(|result| result)
        .map(|message| ActionReport { ok: true, message })
        .unwrap_or_else(|message| ActionReport { ok: false, message });
    if report.ok {
        refresh_desktop_surfaces(&app);
    }
    report
}

fn connect_granola_blocking(api_key: String) -> Result<String, String> {
    let api_key = api_key.trim().to_string();
    if api_key.is_empty() {
        return Err("Enter a Granola API key.".to_string());
    }
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let credentials = open_credential_store(&state_root);
    let report = run_connect_granola(
        &mut store,
        credentials.as_ref(),
        ConnectOptions {
            connection_id: Some(ConnectionId::new("granola-default")),
            token: api_key,
        },
        &HttpGranolaConnectionProbe,
    )
    .map_err(|error| error.message())?;
    let existing_mount = store
        .get_mount(&MountId::new("granola-main"))
        .map_err(|error| format!("Could not inspect Granola mount: {error}"))?
        .filter(|mount| mount.connector == "granola");
    drop(store);
    if let Some(mount) = existing_mount {
        ensure_daemon_running(&state_root)?;
        reload_daemon_mounts(&state_root)?;
        if mount.projection.uses_virtual_filesystem() {
            activate_virtual_projection_mount(&state_root, &mount, true)?;
        }
        return Ok("Reconnected the existing Granola source.".to_string());
    }

    create_desktop_mount_blocking(CreateDesktopMountRequest {
        connector: "granola".to_string(),
        path: "granola".to_string(),
        mount_id: "granola-main".to_string(),
        connection_id: Some(report.connection_id),
        read_only: true,
        notion_root_page: None,
        google_docs_workspace_folder: None,
    })
}

#[tauri::command]
async fn reset_source_state(
    app: AppHandle,
    mount_id: String,
    confirmation: String,
) -> ActionReport {
    let report = tauri::async_runtime::spawn_blocking(move || {
        reset_source_state_blocking(mount_id, confirmation)
    })
    .await
    .map_err(|error| format!("Source reset worker failed: {error}"))
    .and_then(|result| result)
    .map(|message| ActionReport { ok: true, message })
    .unwrap_or_else(|message| ActionReport { ok: false, message });
    if report.ok {
        refresh_desktop_surfaces(&app);
    }
    report
}

fn reset_source_state_blocking(mount_id: String, confirmation: String) -> Result<String, String> {
    let mount_id = MountId::new(mount_id.trim().to_string());
    validate_source_action_confirmation("RESET", &mount_id, &confirmation)?;

    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let mount = store
        .get_mount(&mount_id)
        .map_err(|error| format!("Could not inspect source mount: {error}"))?
        .ok_or_else(|| format!("Source mount `{}` was not found.", mount_id.0))?;
    let credentials = open_credential_store(&state_root);

    // Resolve once before clearing any rebuildable state so a revoked or
    // missing credential cannot turn a recoverable reset into an empty mount.
    resolve_source_for_mount_id(&store, credentials.as_ref(), &mount_id).map_err(|error| {
        format!(
            "Could not access the source before reset: {}",
            error.message()
        )
    })?;
    ensure_virtual_projection_domain_available(&mount.projection)?;
    ensure_daemon_running(&state_root)?;

    let preserved =
        prepare_existing_workspace_mount_for_remount(&mut store, &state_root, &mount_id)?;
    reload_daemon_mounts(&state_root)?;

    // Resolve again after clearing connector checkpoints so reset means a full
    // source refresh, including append-only connectors such as Granola.
    let source =
        resolve_source_for_mount_id(&store, credentials.as_ref(), &mount_id).map_err(|error| {
            format!(
                "Could not prepare the source after reset: {}",
                error.message()
            )
        })?;
    run_pull_with_state_root(&mut store, &source, mount.root.clone(), Some(&state_root)).map_err(
        |error| {
            format!(
                "Could not rebuild source `{}` after reset: {}",
                mount.mount_id.0,
                error.message()
            )
        },
    )?;
    reload_daemon_mounts(&state_root)?;
    if mount.projection.uses_virtual_filesystem() {
        activate_virtual_projection_mount(&state_root, &mount, true)?;
    }

    let mut message = format!(
        "Reset {} source state and rebuilt `{}`.",
        connector_label(&mount.connector),
        absolute_display_path(&mount_access_root(&mount))
    );
    if let Some(preserved) = preserved {
        message.push_str(&format!(
            " Preserved {} pending local change{} at `{}`.",
            preserved.count,
            if preserved.count == 1 { "" } else { "s" },
            preserved.directory.display()
        ));
    }
    Ok(message)
}

#[tauri::command]
async fn disconnect_source(app: AppHandle, mount_id: String, confirmation: String) -> ActionReport {
    let report = tauri::async_runtime::spawn_blocking(move || {
        disconnect_source_blocking(mount_id, confirmation)
    })
    .await
    .map_err(|error| format!("Source disconnect worker failed: {error}"))
    .and_then(|result| result)
    .map(|message| ActionReport { ok: true, message })
    .unwrap_or_else(|message| ActionReport { ok: false, message });
    if report.ok {
        refresh_desktop_surfaces(&app);
    }
    report
}

fn disconnect_source_blocking(mount_id: String, confirmation: String) -> Result<String, String> {
    let mount_id = MountId::new(mount_id.trim().to_string());
    validate_source_action_confirmation("DISCONNECT", &mount_id, &confirmation)?;

    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let mount = store
        .get_mount(&mount_id)
        .map_err(|error| format!("Could not inspect source mount: {error}"))?
        .ok_or_else(|| format!("Source mount `{}` was not found.", mount_id.0))?;
    let connection_id = match mount.connection_id.clone() {
        Some(connection_id) => connection_id,
        None => preferred_connection_id_for_connector(&store, &mount.connector)?
            .ok_or_else(|| format!("Source mount `{}` has no saved connection.", mount_id.0))?,
    };
    let affected_mounts = store
        .load_mounts()
        .map_err(|error| format!("Could not inspect source mounts: {error}"))?
        .into_iter()
        .filter(|candidate| candidate.connection_id.as_ref() == Some(&connection_id))
        .count();
    let credentials = open_credential_store(&state_root);
    run_disconnect(&mut store, credentials.as_ref(), connection_id.clone())
        .map_err(|error| error.message())?;
    ensure_daemon_running(&state_root)?;
    reload_daemon_mounts(&state_root)?;

    let mut message = format!(
        "Disconnected {} connection `{}`. The local source folder remains registered for reconnection.",
        connector_label(&mount.connector),
        connection_id.0
    );
    if affected_mounts > 1 {
        message.push_str(&format!(
            " This connection was shared by {affected_mounts} source mounts, which now need reconnection."
        ));
    }
    Ok(message)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceBackupManifest {
    generated_at: String,
    mount_id: String,
    connector: String,
    connector_name: String,
    source_path: String,
    backup_path: String,
    files_path: String,
    file_count: usize,
    directory_count: usize,
    bytes: u64,
    skipped: Vec<SourceBackupSkippedItem>,
    note: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceBackupSkippedItem {
    path: String,
    reason: String,
}

#[derive(Default)]
struct SourceBackupStats {
    files: usize,
    directories: usize,
    bytes: u64,
    skipped: Vec<SourceBackupSkippedItem>,
}

struct SourceBackupExport {
    directory: PathBuf,
    stats: SourceBackupStats,
    mount: MountConfig,
}

impl SourceBackupExport {
    fn skipped_count(&self) -> usize {
        self.stats.skipped.len()
    }
}

#[tauri::command]
async fn export_source_backup(app: AppHandle, mount_id: String) -> ActionReport {
    let report =
        tauri::async_runtime::spawn_blocking(move || export_source_backup_blocking(mount_id))
            .await
            .map_err(|error| format!("Source backup worker failed: {error}"))
            .and_then(|result| result)
            .map(|message| ActionReport { ok: true, message })
            .unwrap_or_else(|message| ActionReport { ok: false, message });
    if report.ok {
        refresh_desktop_surfaces(&app);
    }
    report
}

fn export_source_backup_blocking(mount_id: String) -> Result<String, String> {
    let state_root = default_state_root();
    let store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let backup_root = source_backup_root()?;
    let export = export_source_backup_to_root(&store, &backup_root, &mount_id)?;
    let skipped = export.skipped_count();
    let skipped_note = if skipped > 0 {
        format!(
            " {skipped} item{} could not be copied; see manifest.json.",
            if skipped == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    let message = format!(
        "Exported {} source backup to `{}` ({} file{}).{}",
        connector_label(&export.mount.connector),
        display_path(&export.directory),
        export.stats.files,
        if export.stats.files == 1 { "" } else { "s" },
        skipped_note
    );
    if let Err(error) = record_desktop_activity(
        &state_root,
        "Exported source backup",
        &format!(
            "{} source `{}` was exported to `{}`.",
            connector_label(&export.mount.connector),
            export.mount.mount_id.0,
            display_path(&export.directory)
        ),
        "backup",
    ) {
        desktop_log(
            "warn",
            "source_backup.activity_failed",
            format!("could not record source backup activity: {error}"),
        );
    }
    Ok(message)
}

fn export_source_backup_to_root(
    store: &SqliteStateStore,
    backup_root: &Path,
    mount_id: &str,
) -> Result<SourceBackupExport, String> {
    let mount_id = MountId::new(mount_id.trim().to_string());
    if mount_id.0.is_empty() {
        return Err("Source mount id is required.".to_string());
    }
    let mount = store
        .get_mount(&mount_id)
        .map_err(|error| format!("Could not inspect source mount: {error}"))?
        .ok_or_else(|| format!("Source mount `{}` was not found.", mount_id.0))?;
    let source_root = mount_access_root(&mount);
    if !source_root.exists() {
        return Err(format!(
            "Source folder `{}` does not exist yet. Open or sync the source before exporting a backup.",
            display_path(&source_root)
        ));
    }
    if !source_root.is_dir() {
        return Err(format!(
            "Source folder `{}` is not a directory.",
            display_path(&source_root)
        ));
    }

    let backup_dir = unique_source_backup_dir(backup_root, &mount_id.0);
    let files_dir = backup_dir.join("files");
    fs::create_dir_all(&files_dir)
        .map_err(|error| format!("Could not create source backup folder: {error}"))?;
    let stats = copy_source_backup_tree(&source_root, &files_dir)?;
    write_source_backup_readme(&backup_dir, &mount, &source_root, &files_dir, &stats)?;
    write_source_backup_manifest(&backup_dir, &mount, &source_root, &files_dir, &stats)?;

    Ok(SourceBackupExport {
        directory: backup_dir,
        stats,
        mount,
    })
}

fn source_backup_root() -> Result<PathBuf, String> {
    home_dir()
        .map(|home| home.join("Locality Backups"))
        .map_err(|error| format!("Could not find a home folder for source backups: {error}"))
}

fn unique_source_backup_dir(backup_root: &Path, mount_id: &str) -> PathBuf {
    let source_root = backup_root.join(safe_backup_segment(mount_id));
    let timestamp = source_backup_timestamp();
    let mut candidate = source_root.join(&timestamp);
    let mut suffix = 2;
    while candidate.exists() {
        candidate = source_root.join(format!("{timestamp}-{suffix}"));
        suffix += 1;
    }
    candidate
}

fn source_backup_timestamp() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("backup-{millis}")
}

fn safe_backup_segment(value: &str) -> String {
    let mut output = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    while output.contains("--") {
        output = output.replace("--", "-");
    }
    let output = output.trim_matches('-').to_string();
    if output.is_empty() {
        "source".to_string()
    } else {
        output
    }
}

fn copy_source_backup_tree(
    source_root: &Path,
    files_dir: &Path,
) -> Result<SourceBackupStats, String> {
    let mut stats = SourceBackupStats::default();
    let entries = fs::read_dir(source_root).map_err(|error| {
        format!(
            "Could not read source folder `{}` for backup: {error}",
            display_path(source_root)
        )
    })?;
    for entry in entries {
        match entry {
            Ok(entry) => {
                let destination = files_dir.join(entry.file_name());
                copy_source_backup_entry(source_root, &entry.path(), &destination, &mut stats);
            }
            Err(error) => stats.skipped.push(SourceBackupSkippedItem {
                path: ".".to_string(),
                reason: format!("Could not read source folder entry: {error}"),
            }),
        }
    }
    Ok(stats)
}

fn copy_source_backup_entry(
    source_root: &Path,
    source_path: &Path,
    destination_path: &Path,
    stats: &mut SourceBackupStats,
) {
    let relative_path = source_backup_relative_path(source_root, source_path);
    let metadata = match fs::symlink_metadata(source_path) {
        Ok(metadata) => metadata,
        Err(error) => {
            stats.skipped.push(SourceBackupSkippedItem {
                path: relative_path,
                reason: format!("Could not inspect item: {error}"),
            });
            return;
        }
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        stats.skipped.push(SourceBackupSkippedItem {
            path: relative_path,
            reason: "Symbolic links are skipped so backups stay self-contained.".to_string(),
        });
        return;
    }
    if metadata.is_dir() {
        if let Err(error) = fs::create_dir_all(destination_path) {
            stats.skipped.push(SourceBackupSkippedItem {
                path: relative_path,
                reason: format!("Could not create backup directory: {error}"),
            });
            return;
        }
        stats.directories += 1;
        let entries = match fs::read_dir(source_path) {
            Ok(entries) => entries,
            Err(error) => {
                stats.skipped.push(SourceBackupSkippedItem {
                    path: relative_path,
                    reason: format!("Could not read directory: {error}"),
                });
                return;
            }
        };
        for entry in entries {
            match entry {
                Ok(entry) => {
                    copy_source_backup_entry(
                        source_root,
                        &entry.path(),
                        &destination_path.join(entry.file_name()),
                        stats,
                    );
                }
                Err(error) => stats.skipped.push(SourceBackupSkippedItem {
                    path: relative_path.clone(),
                    reason: format!("Could not read directory entry: {error}"),
                }),
            }
        }
        return;
    }
    if metadata.is_file() {
        if let Some(parent) = destination_path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                stats.skipped.push(SourceBackupSkippedItem {
                    path: relative_path,
                    reason: format!("Could not create backup parent folder: {error}"),
                });
                return;
            }
        }
        match fs::copy(source_path, destination_path) {
            Ok(bytes) => {
                stats.files += 1;
                stats.bytes = stats.bytes.saturating_add(bytes);
            }
            Err(error) => stats.skipped.push(SourceBackupSkippedItem {
                path: relative_path,
                reason: format!("Could not copy file: {error}"),
            }),
        }
        return;
    }

    stats.skipped.push(SourceBackupSkippedItem {
        path: relative_path,
        reason: "Unsupported filesystem item type.".to_string(),
    });
}

fn source_backup_relative_path(source_root: &Path, source_path: &Path) -> String {
    source_path
        .strip_prefix(source_root)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| ".".to_string())
}

fn write_source_backup_readme(
    backup_dir: &Path,
    mount: &MountConfig,
    source_root: &Path,
    files_dir: &Path,
    stats: &SourceBackupStats,
) -> Result<(), String> {
    let skipped_note = if stats.skipped.is_empty() {
        "No items were skipped.".to_string()
    } else {
        format!(
            "{} item{} could not be copied. See `manifest.json` for details.",
            stats.skipped.len(),
            if stats.skipped.len() == 1 { "" } else { "s" }
        )
    };
    let contents = format!(
        "# Locality Source Backup\n\n\
This folder is a point-in-time export of the `{}` source mounted at `{}`.\n\n\
- Files are copied under `files/`.\n\
- `manifest.json` records the source, destination, copied file count, and skipped items.\n\
- Editing this backup does not sync back to {}. Use the mounted source folder for live work.\n\n\
Copied {} file{} from `{}` into `{}`.\n{}\n",
        mount.mount_id.0,
        display_path(source_root),
        connector_label(&mount.connector),
        stats.files,
        if stats.files == 1 { "" } else { "s" },
        display_path(source_root),
        display_path(files_dir),
        skipped_note
    );
    fs::write(backup_dir.join("README.md"), contents)
        .map_err(|error| format!("Could not write source backup README: {error}"))
}

fn write_source_backup_manifest(
    backup_dir: &Path,
    mount: &MountConfig,
    source_root: &Path,
    files_dir: &Path,
    stats: &SourceBackupStats,
) -> Result<(), String> {
    let manifest = SourceBackupManifest {
        generated_at: activity_timestamp(),
        mount_id: mount.mount_id.0.clone(),
        connector: mount.connector.clone(),
        connector_name: connector_label(&mount.connector),
        source_path: source_root.display().to_string(),
        backup_path: backup_dir.display().to_string(),
        files_path: files_dir.display().to_string(),
        file_count: stats.files,
        directory_count: stats.directories,
        bytes: stats.bytes,
        skipped: stats.skipped.clone(),
        note: "This backup is a local export only. It does not sync back to the remote source."
            .to_string(),
    };
    let contents = serde_json::to_string_pretty(&manifest)
        .map_err(|error| format!("Could not serialize source backup manifest: {error}"))?;
    fs::write(backup_dir.join("manifest.json"), contents)
        .map_err(|error| format!("Could not write source backup manifest: {error}"))
}

fn validate_source_action_confirmation(
    action: &str,
    mount_id: &MountId,
    confirmation: &str,
) -> Result<(), String> {
    if mount_id.0.trim().is_empty() {
        return Err("Mount id is required.".to_string());
    }
    let required = format!("{action} {}", mount_id.0);
    if confirmation.trim() != required {
        return Err(format!("Type `{required}` to confirm this source action."));
    }
    Ok(())
}

#[tauri::command]
async fn run_workspace_mount_onboarding(
    app: AppHandle,
    request: WorkspaceMountOnboardingRequest,
) -> WorkspaceMountOnboardingReport {
    let report = tauri::async_runtime::spawn_blocking(move || {
        run_workspace_mount_onboarding_blocking(request)
    })
    .await
    .unwrap_or_else(|error| {
        workspace_mount_onboarding_report(
            MacosWorkspaceMountOnboardingState::Failed,
            format!("Mount onboarding worker failed: {error}"),
            WorkspaceMountOnboardingPrimaryAction::RetrySetup,
            WorkspaceMountOnboardingLaunchStrategy::None,
        )
    });
    if workspace_mount_onboarding_should_refresh_surfaces(&report) {
        refresh_desktop_surfaces(&app);
    }
    report
}

#[tauri::command]
async fn file_provider_enablement_status() -> FileProviderEnablementReport {
    tauri::async_runtime::spawn_blocking(macos_file_provider_enablement_status_blocking)
        .await
        .unwrap_or_else(|error| FileProviderEnablementReport {
            state: "unavailable".to_string(),
            message: format!("File Provider status worker failed: {error}"),
            path: None,
        })
}

#[tauri::command]
async fn reveal_file_provider_enablement() -> ActionReport {
    tauri::async_runtime::spawn_blocking(reveal_file_provider_enablement_blocking)
        .await
        .map_or_else(
            |error| ActionReport {
                ok: false,
                message: format!("Finder worker failed: {error}"),
            },
            |result| match result {
                Ok(message) => ActionReport { ok: true, message },
                Err(message) => ActionReport { ok: false, message },
            },
        )
}

async fn create_desktop_mount_command(
    app: AppHandle,
    request: CreateDesktopMountRequest,
) -> ActionReport {
    let report =
        match tauri::async_runtime::spawn_blocking(move || create_desktop_mount_blocking(request))
            .await
            .map_err(|error| format!("Mount worker failed: {error}"))
            .and_then(|result| result)
        {
            Ok(message) => ActionReport { ok: true, message },
            Err(message) => ActionReport { ok: false, message },
        };
    if report.ok {
        refresh_desktop_surfaces(&app);
    }
    report
}

fn run_workspace_mount_onboarding_blocking(
    request: WorkspaceMountOnboardingRequest,
) -> WorkspaceMountOnboardingReport {
    let action = match WorkspaceMountOnboardingAction::parse(request.action.trim()) {
        Ok(action) => action,
        Err(message) => {
            return workspace_mount_onboarding_report(
                MacosWorkspaceMountOnboardingState::Failed,
                message,
                WorkspaceMountOnboardingPrimaryAction::RetrySetup,
                WorkspaceMountOnboardingLaunchStrategy::None,
            );
        }
    };

    if matches!(action, WorkspaceMountOnboardingAction::AllowInMacos) {
        #[cfg(target_os = "macos")]
        {
            let launch_strategy = launch_macos_file_provider_approval_surface();
            return workspace_mount_onboarding_report(
                MacosWorkspaceMountOnboardingState::ApprovalRequired,
                workspace_mount_onboarding_curated_message(
                    MacosWorkspaceMountOnboardingState::ApprovalRequired,
                )
                .expect("approval_required message"),
                WorkspaceMountOnboardingPrimaryAction::CheckAgain,
                launch_strategy,
            );
        }
        #[cfg(not(target_os = "macos"))]
        {
            return workspace_mount_onboarding_report(
                MacosWorkspaceMountOnboardingState::Failed,
                "macOS File Provider approval is only available on macOS.",
                WorkspaceMountOnboardingPrimaryAction::RetrySetup,
                WorkspaceMountOnboardingLaunchStrategy::None,
            );
        }
    }

    match create_desktop_mount_blocking(CreateDesktopMountRequest {
        connector: "notion".to_string(),
        path: request.path,
        mount_id: "notion-main".to_string(),
        connection_id: None,
        read_only: false,
        notion_root_page: None,
        google_docs_workspace_folder: None,
    }) {
        Ok(message) => workspace_mount_onboarding_report(
            MacosWorkspaceMountOnboardingState::Created,
            message,
            WorkspaceMountOnboardingPrimaryAction::RetrySetup,
            WorkspaceMountOnboardingLaunchStrategy::None,
        ),
        Err(message) => classify_workspace_mount_onboarding_failure(&message),
    }
}

#[tauri::command]
async fn install_agent_guidance(mount_path: Option<String>) -> AgentGuidanceInstallReport {
    tauri::async_runtime::spawn_blocking(move || install_guidance_files(mount_path.as_deref()))
        .await
        .unwrap_or_else(|error| AgentGuidanceInstallReport {
            ok: false,
            command: "install_agent_guidance",
            targets: vec![agent_guidance::AgentGuidanceTarget {
                agent: "Agent instructions".to_string(),
                status: "failed".to_string(),
                path: None,
                detail: format!("Agent guidance worker failed: {error}"),
            }],
            prompt: String::new(),
        })
}

#[tauri::command]
async fn locate_notion_page(url: String) -> Result<LocatedItem, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let query = url.trim();
        if query.is_empty() {
            return Err(
                "Paste a Notion page or database URL, or search your local Notion index."
                    .to_string(),
            );
        }

        locate_notion_query(query)
    })
    .await
    .map_err(|error| format!("Locate worker failed: {error}"))?
}

#[tauri::command]
async fn search_notion_pages(query: String) -> Result<Vec<LocatedItem>, String> {
    tauri::async_runtime::spawn_blocking(move || search_notion_index(&query, 8))
        .await
        .map_err(|error| format!("Search worker failed: {error}"))?
}

#[tauri::command]
async fn review_push_plan() -> PushPlan {
    let files = tauri::async_runtime::spawn_blocking(load_desktop_snapshot)
        .await
        .ok()
        .and_then(Result::ok)
        .map(|snapshot| snapshot.pending_changes)
        .unwrap_or_default();
    let pages_updated = files.len();
    PushPlan {
        title: "Review Push".to_string(),
        summary: format!("{pages_updated} files will update Notion."),
        pages_updated,
        database_rows_updated: 0,
        pages_deleted: 0,
        can_push: pages_updated > 0,
        guardrail_state: "safe".to_string(),
        files,
    }
}

#[tauri::command]
async fn push_to_notion(app: AppHandle, confirm_dangerous: Option<bool>) -> ActionReport {
    let report = match tauri::async_runtime::spawn_blocking(move || {
        push_to_notion_blocking(confirm_dangerous.unwrap_or(false))
    })
    .await
    {
        Ok(report) => report,
        Err(error) => ActionReport {
            ok: false,
            message: format!("Push worker failed: {error}"),
        },
    };

    refresh_desktop_surfaces(&app);
    report
}

#[tauri::command]
async fn push_notion_file(
    app: AppHandle,
    path: String,
    confirm_dangerous: Option<bool>,
) -> ActionReport {
    let report = match tauri::async_runtime::spawn_blocking(move || {
        let target = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
        match push_target_direct(&target, confirm_dangerous.unwrap_or(false)) {
            Ok(report) => ActionReport {
                ok: push_report_exit_code(&report) == 0,
                message: push_report_message(&report),
            },
            Err(message) => ActionReport { ok: false, message },
        }
    })
    .await
    {
        Ok(report) => report,
        Err(error) => ActionReport {
            ok: false,
            message: format!("Push worker failed: {error}"),
        },
    };

    refresh_desktop_surfaces(&app);
    report
}

#[tauri::command]
async fn pull_notion_file(app: AppHandle, path: String) -> ActionReport {
    let report = match tauri::async_runtime::spawn_blocking(move || {
        let target = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
        match pull_target_direct(&target) {
            Ok(report) => ActionReport {
                ok: true,
                message: pull_report_message(&report),
            },
            Err(message) => ActionReport {
                ok: false,
                message: pull_error_message(&message),
            },
        }
    })
    .await
    {
        Ok(report) => report,
        Err(error) => ActionReport {
            ok: false,
            message: format!("Pull worker failed: {error}"),
        },
    };

    refresh_desktop_surfaces(&app);
    report
}

#[tauri::command]
async fn check_notion_file(app: AppHandle, path: String) -> ActionReport {
    let report = match tauri::async_runtime::spawn_blocking(move || {
        let target = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
        match queue_observe_target_direct(&target) {
            Ok(()) => ActionReport {
                ok: true,
                message: "Checking Notion for this page.".to_string(),
            },
            Err(message) => ActionReport { ok: false, message },
        }
    })
    .await
    {
        Ok(report) => report,
        Err(error) => ActionReport {
            ok: false,
            message: format!("Check worker failed: {error}"),
        },
    };

    refresh_desktop_surfaces(&app);
    report
}

#[tauri::command]
async fn keep_notion_file_as_draft(app: AppHandle, path: String) -> ActionReport {
    let report = match tauri::async_runtime::spawn_blocking(move || {
        let target = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
        match keep_target_as_local_draft_direct(&target) {
            Ok(draft_path) => ActionReport {
                ok: true,
                message: format!(
                    "Kept a local draft at `{}` and removed the deleted Notion page from Locality.",
                    draft_path.display()
                ),
            },
            Err(message) => ActionReport { ok: false, message },
        }
    })
    .await
    {
        Ok(report) => report,
        Err(error) => ActionReport {
            ok: false,
            message: format!("Draft worker failed: {error}"),
        },
    };

    refresh_desktop_surfaces(&app);
    report
}

#[tauri::command]
async fn reset_notion_file_to_remote(app: AppHandle, path: String) -> ActionReport {
    let report = match tauri::async_runtime::spawn_blocking(move || {
        let target = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
        match reset_target_to_remote_direct(&target) {
            Ok(report) => ActionReport {
                ok: report.ok && report.skipped_dirty == 0 && report.conflicts.is_empty(),
                message: reset_to_remote_message(&report),
            },
            Err(message) => ActionReport { ok: false, message },
        }
    })
    .await
    {
        Ok(report) => report,
        Err(error) => ActionReport {
            ok: false,
            message: format!("Reset worker failed: {error}"),
        },
    };

    refresh_desktop_surfaces(&app);
    report
}

#[tauri::command]
async fn live_mode_tick(app: AppHandle) -> ActionReport {
    let report = match tauri::async_runtime::spawn_blocking(live_mode_tick_blocking).await {
        Ok(report) => report,
        Err(error) => ActionReport {
            ok: false,
            message: format!("Live Mode worker failed: {error}"),
        },
    };

    refresh_desktop_surfaces(&app);
    report
}

#[tauri::command]
async fn set_mount_live_mode(app: AppHandle, change: MountLiveModeChange) -> ActionReport {
    let report = match tauri::async_runtime::spawn_blocking(move || {
        set_mount_live_mode_blocking(change).unwrap_or_else(action_error)
    })
    .await
    {
        Ok(report) => report,
        Err(error) => ActionReport {
            ok: false,
            message: format!("Live Mode worker failed: {error}"),
        },
    };

    refresh_desktop_surfaces(&app);
    report
}

#[tauri::command]
async fn diff_notion_file(path: String) -> ActionReport {
    match tauri::async_runtime::spawn_blocking(move || {
        let target = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
        match diff_target_direct(&target) {
            Ok(report) => ActionReport {
                ok: report.ok,
                message: diff_report_message(&report),
            },
            Err(message) => ActionReport { ok: false, message },
        }
    })
    .await
    {
        Ok(report) => report,
        Err(error) => ActionReport {
            ok: false,
            message: format!("Diff worker failed: {error}"),
        },
    }
}

#[tauri::command]
async fn inspect_notion_file(path: String) -> FileDetailReport {
    let fallback_path = path.clone();
    match tauri::async_runtime::spawn_blocking(move || {
        let target = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
        inspect_notion_file_blocking(&target)
    })
    .await
    {
        Ok(report) => report,
        Err(error) => FileDetailReport {
            ok: false,
            path: fallback_path,
            has_conflict_markers: false,
            conflict_preview: None,
            message: format!("Inspect worker failed: {error}"),
        },
    }
}

#[tauri::command]
async fn read_notion_file(path: String) -> FileEditorReport {
    let fallback_path = path.clone();
    match tauri::async_runtime::spawn_blocking(move || {
        let target = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
        read_notion_file_blocking(&target)
    })
    .await
    {
        Ok(report) => report,
        Err(error) => FileEditorReport {
            ok: false,
            path: fallback_path,
            contents: String::new(),
            has_conflict_markers: false,
            message: format!("Read worker failed: {error}"),
        },
    }
}

#[tauri::command]
async fn save_notion_file(app: AppHandle, path: String, contents: String) -> ActionReport {
    let report = match tauri::async_runtime::spawn_blocking(move || {
        let target = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
        match save_notion_file_blocking(&target, &contents) {
            Ok(message) => ActionReport { ok: true, message },
            Err(message) => ActionReport { ok: false, message },
        }
    })
    .await
    {
        Ok(report) => report,
        Err(error) => ActionReport {
            ok: false,
            message: format!("Save worker failed: {error}"),
        },
    };

    refresh_desktop_surfaces(&app);
    report
}

fn push_to_notion_blocking(confirm_dangerous: bool) -> ActionReport {
    let Ok(snapshot) = load_desktop_snapshot() else {
        return ActionReport {
            ok: false,
            message: "No Locality mount is available to push.".to_string(),
        };
    };
    if snapshot.pending_changes.is_empty() {
        return ActionReport {
            ok: true,
            message: "No pending changes to push.".to_string(),
        };
    };

    let mut pushed = 0usize;
    for change in &snapshot.pending_changes {
        let target = expand_tilde(&join_mount_path(
            &snapshot.mount.local_path,
            &change.local_path,
        ))
        .unwrap_or_else(|_| PathBuf::from(&change.local_path));
        match push_target_direct(&target, confirm_dangerous) {
            Ok(report) if push_report_exit_code(&report) == 0 => {
                pushed += 1;
            }
            Ok(report) => {
                return ActionReport {
                    ok: false,
                    message: push_report_message(&report),
                };
            }
            Err(message) => return ActionReport { ok: false, message },
        }
    }

    ActionReport {
        ok: true,
        message: format!(
            "Pushed {pushed} pending change{} to Notion.",
            if pushed == 1 { "" } else { "s" }
        ),
    }
}

fn live_mode_tick_blocking() -> ActionReport {
    if LIVE_MODE_TICK_IN_PROGRESS
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return ActionReport {
            ok: true,
            message: "Live Mode is already syncing.".to_string(),
        };
    }

    let state_root = default_state_root();
    let report = live_mode_tick_blocking_at_state_root(&state_root);
    LIVE_MODE_TICK_IN_PROGRESS.store(false, Ordering::Release);
    report
}

fn live_mode_tick_blocking_at_state_root(state_root: &Path) -> ActionReport {
    let mount = match live_mode_enabled_mount(&state_root) {
        Ok(Some(mount)) => mount,
        Ok(None) => {
            return ActionReport {
                ok: true,
                message: "Live Mode is off.".to_string(),
            };
        }
        Err(message) => {
            return ActionReport { ok: false, message };
        }
    };

    match mark_mount_live_mode_syncing(&state_root, &mount.mount_id) {
        Ok(true) => {}
        Ok(false) => {
            return ActionReport {
                ok: true,
                message: "Live Mode is off.".to_string(),
            };
        }
        Err(message) => return ActionReport { ok: false, message },
    }

    let report = live_mode_tick_for_enabled_mount(&state_root, &mount);
    if let Err(message) = record_mount_live_mode_tick_result(&state_root, &mount.mount_id, &report)
    {
        return ActionReport { ok: false, message };
    }

    report
}

fn live_mode_tick_for_enabled_mount(state_root: &Path, mount: &MountConfig) -> ActionReport {
    if let Err(message) = live_mode_reconcile_recent_local_targets(state_root, mount) {
        return ActionReport { ok: false, message };
    }

    match load_desktop_snapshot_at_state_root(state_root) {
        Ok(mut snapshot) => {
            let remote_targets = if snapshot.pending_changes.is_empty()
                && live_mode_remote_pull_scan_is_due(mount)
            {
                match live_mode_next_remote_pull_targets_for_state_root(
                    state_root,
                    mount,
                    live_mode_remote_check_page_budget(),
                ) {
                    Ok(target) => target,
                    Err(message) => {
                        return ActionReport { ok: false, message };
                    }
                }
            } else {
                Vec::new()
            };
            for target in &remote_targets {
                if live_mode_should_reconcile_local_target(target)
                    && let Err(message) = live_mode_reconcile_local_target(target).map(|_| ())
                {
                    return ActionReport { ok: false, message };
                }
            }
            if !remote_targets.is_empty() {
                snapshot = match load_desktop_snapshot_at_state_root(state_root) {
                    Ok(snapshot) => snapshot,
                    Err(message) => {
                        return ActionReport {
                            ok: false,
                            message: format!(
                                "Live Mode could not re-check local changes before remote work: {message}"
                            ),
                        };
                    }
                };
            }

            live_mode_tick_from_snapshot(
                &snapshot,
                &remote_targets,
                |_change, target| {
                    desktop_log(
                        "info",
                        "live_mode.local_push_started",
                        format!("pushing `{}` from Live Mode", target.display()),
                    );
                    let started = Instant::now();
                    match push_target_direct_at_state_root(state_root, target, false) {
                        Ok(push_report) if push_report_exit_code(&push_report) == 0 => {
                            desktop_log(
                                "info",
                                "live_mode.local_push_completed",
                                format!(
                                    "pushed `{}` from Live Mode in {} ms",
                                    target.display(),
                                    started.elapsed().as_millis()
                                ),
                            );
                            Ok(())
                        }
                        Ok(push_report) => {
                            let message = push_report_message(&push_report);
                            desktop_log(
                                "warn",
                                "live_mode.local_push_failed",
                                format!("push failed for `{}`: {message}", target.display()),
                            );
                            Err(message)
                        }
                        Err(message) => {
                            desktop_log(
                                "warn",
                                "live_mode.local_push_failed",
                                format!("push failed for `{}`: {message}", target.display()),
                            );
                            Err(message)
                        }
                    }
                },
                |target| live_mode_queue_remote_fast_forward_at_state_root(state_root, target),
                live_mode_merge_remote_drift_target,
            )
        }
        Err(message) => ActionReport {
            ok: false,
            message: format!("Live Mode could not inspect the desktop state: {message}"),
        },
    }
}

fn live_mode_tick_from_snapshot<Sync, FastForward, Merge>(
    snapshot: &DesktopSnapshot,
    remote_pull_targets: &[LiveModeRemoteTarget],
    mut sync_target: Sync,
    mut fast_forward_remote_target: FastForward,
    mut merge_remote_drift: Merge,
) -> ActionReport
where
    Sync: FnMut(&PendingChange, &Path) -> Result<(), String>,
    FastForward: FnMut(&LiveModeRemoteTarget) -> Result<(), String>,
    Merge: FnMut(&PendingChange, &Path) -> Result<LiveModeRemoteDriftMerge, String>,
{
    if !live_mode_has_mounted_folder(snapshot) {
        return ActionReport {
            ok: false,
            message: "Live Mode needs a mounted Notion mount point.".to_string(),
        };
    }

    let remote_only_pending_targets = match live_mode_remote_only_pending_targets(snapshot) {
        Ok(targets) => targets,
        Err(message) => return ActionReport { ok: false, message },
    };
    if !remote_only_pending_targets.is_empty() {
        for target in &remote_only_pending_targets {
            if let Err(message) = fast_forward_remote_target(target) {
                return ActionReport { ok: false, message };
            }
        }
        return ActionReport {
            ok: true,
            message: live_mode_queued_remote_updates_message(remote_only_pending_targets.len()),
        };
    }

    let Some(change) = snapshot.pending_changes.first() else {
        if !remote_pull_targets.is_empty() {
            for target in remote_pull_targets {
                if let Err(message) = fast_forward_remote_target(target) {
                    return ActionReport { ok: false, message };
                }
            }
            return ActionReport {
                ok: true,
                message: live_mode_queued_remote_updates_message(remote_pull_targets.len()),
            };
        }

        return ActionReport {
            ok: true,
            message: "Live Mode checked for changes.".to_string(),
        };
    };
    let target = expand_tilde(&join_mount_path(
        &snapshot.mount.local_path,
        &change.local_path,
    ))
    .unwrap_or_else(|_| PathBuf::from(&change.local_path));

    if change.state != "safe" {
        if live_mode_change_may_merge_remote_drift(change) {
            match merge_remote_drift(change, &target) {
                Ok(LiveModeRemoteDriftMerge::Clean) => {
                    if let Err(message) = sync_target(change, &target) {
                        return ActionReport { ok: false, message };
                    }
                    return ActionReport {
                        ok: true,
                        message: "Live Mode merged remote updates and synced 1 pending change."
                            .to_string(),
                    };
                }
                Ok(LiveModeRemoteDriftMerge::ConflictMarkersWritten) => {
                    return ActionReport {
                        ok: true,
                        message: format!(
                            "Live Mode pulled remote updates for `{}` and wrote conflict markers for review.",
                            change.title
                        ),
                    };
                }
                Ok(LiveModeRemoteDriftMerge::Unchanged) => {}
                Err(message) => return ActionReport { ok: false, message },
            }
        }
        return ActionReport {
            ok: false,
            message: format!(
                "Live Mode paused for `{}`: {}.",
                change.title, change.summary
            ),
        };
    }

    if let Err(message) = sync_target(change, &target) {
        return ActionReport { ok: false, message };
    }

    ActionReport {
        ok: true,
        message: "Live Mode synced 1 pending change.".to_string(),
    }
}

fn live_mode_remote_only_pending_targets(
    snapshot: &DesktopSnapshot,
) -> Result<Vec<LiveModeRemoteTarget>, String> {
    let mut targets = Vec::new();
    for change in &snapshot.pending_changes {
        if !live_mode_change_is_remote_update_only(change) {
            continue;
        }
        let target_path = expand_tilde(&join_mount_path(
            &snapshot.mount.local_path,
            &change.local_path,
        ))
        .unwrap_or_else(|_| PathBuf::from(&change.local_path));
        targets.push(live_mode_remote_target_for_pending_change(
            change,
            &target_path,
        )?);
    }
    Ok(targets)
}

fn live_mode_queued_remote_updates_message(count: usize) -> String {
    format!(
        "Live Mode queued {count} remote {}.",
        if count == 1 { "update" } else { "updates" }
    )
}

fn live_mode_change_is_remote_update_only(change: &PendingChange) -> bool {
    matches!(
        change.state.as_str(),
        "needs_review" | "remote_update_available"
    ) && change
        .issue_codes
        .iter()
        .any(|code| code == "remote_changed")
}

fn live_mode_remote_target_for_pending_change(
    change: &PendingChange,
    path: &Path,
) -> Result<LiveModeRemoteTarget, String> {
    if change.mount_id.trim().is_empty() || change.entity_id.trim().is_empty() {
        return Err(format!(
            "Live Mode could not identify the remote page for `{}`.",
            change.title
        ));
    }
    Ok(LiveModeRemoteTarget {
        mount_id: MountId::new(change.mount_id.clone()),
        remote_id: RemoteId::new(change.entity_id.clone()),
        path: path.to_path_buf(),
    })
}

fn live_mode_change_may_merge_remote_drift(change: &PendingChange) -> bool {
    if change.state != "needs_review" {
        return false;
    }
    change
        .issue_codes
        .iter()
        .any(|code| code == "remote_changed_with_local_pending")
        || change
            .summary
            .to_ascii_lowercase()
            .contains("remote changed while local edits are pending")
}

fn live_mode_has_mounted_folder(snapshot: &DesktopSnapshot) -> bool {
    snapshot.mount.status != "not_mounted" && !snapshot.mount.local_path.trim().is_empty()
}

fn live_mode_enabled_mount(state_root: &Path) -> Result<Option<MountConfig>, String> {
    let mut store = SqliteStateStore::open(state_root.to_path_buf())
        .map_err(|error| format!("Live Mode could not open Locality state: {error}"))?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Live Mode could not inspect mounted folders: {error}"))?;
    let connections = store
        .list_connections()
        .map_err(|error| format!("Live Mode could not inspect connections: {error}"))?;
    let Some(mount) = choose_mount(&mounts, &connections) else {
        return Ok(None);
    };
    let Some(record) = store
        .get_mount_live_mode(&mount.mount_id)
        .map_err(|error| format!("Live Mode could not inspect its state: {error}"))?
    else {
        return Ok(None);
    };
    if record.enabled {
        return Ok(Some(mount));
    }
    if live_mode_resume_stale_disabled_pause_if_safe(&mut store, state_root, &mount, &record)? {
        return Ok(Some(mount));
    }
    Ok(None)
}

fn live_mode_resume_stale_disabled_pause_if_safe(
    store: &mut SqliteStateStore,
    state_root: &Path,
    mount: &MountConfig,
    record: &MountLiveModeRecord,
) -> Result<bool, String> {
    if !live_mode_disabled_pause_can_resume(record) {
        return Ok(false);
    }
    let pending_changes = pending_changes_for_mount(store, state_root, &mount.mount_id)?;
    if pending_changes.is_empty() || pending_changes.iter().any(|change| change.state != "safe") {
        return Ok(false);
    }

    let now = live_mode_timestamp();
    let resumed = record.clone().active(
        Some("Live Mode resumed after remote drift cleared.".to_string()),
        now.clone(),
        now,
    );
    store.save_mount_live_mode(resumed).map_err(|error| {
        format!("Live Mode could not resume after remote drift cleared: {error}")
    })?;
    Ok(true)
}

fn live_mode_disabled_pause_can_resume(record: &MountLiveModeRecord) -> bool {
    !record.enabled
        && record.state == MountLiveModeState::Error
        && record.last_reason.as_deref().is_some_and(|reason| {
            live_mode_failure_should_pause(reason)
                && reason.contains("remote changed while local edits are pending")
        })
}

#[derive(Debug, Default)]
struct LiveModeWakeState {
    generation: u64,
}

fn live_mode_wake_state() -> &'static (Mutex<LiveModeWakeState>, Condvar) {
    static STATE: OnceLock<(Mutex<LiveModeWakeState>, Condvar)> = OnceLock::new();
    STATE.get_or_init(|| (Mutex::new(LiveModeWakeState::default()), Condvar::new()))
}

fn live_mode_wake_generation() -> u64 {
    let (state, _) = live_mode_wake_state();
    state
        .lock()
        .expect("live mode wake lock poisoned")
        .generation
}

fn wake_live_mode_runner() {
    let (state, cvar) = live_mode_wake_state();
    {
        let mut state = state.lock().expect("live mode wake lock poisoned");
        state.generation = state.generation.wrapping_add(1);
    }
    cvar.notify_all();
}

fn log_live_mode_state_signal_error(error: impl std::fmt::Display) {
    desktop_log(
        "warn",
        "live_mode.signal_failed",
        format!("could not publish Live Mode state-change signal: {error}"),
    );
}

fn wait_for_live_mode_state_change(last_seen: &mut u64, timeout: Duration) -> bool {
    let (state, cvar) = live_mode_wake_state();
    let guard = state.lock().expect("live mode wake lock poisoned");
    let (guard, timeout) = cvar
        .wait_timeout_while(guard, timeout, |state| state.generation == *last_seen)
        .expect("live mode wake lock poisoned");
    let changed = guard.generation != *last_seen;
    *last_seen = guard.generation;
    changed && !timeout.timed_out()
}

fn set_mount_live_mode_blocking(change: MountLiveModeChange) -> Result<ActionReport, String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Could not inspect mounted folders: {error}"))?;
    let connections = store
        .list_connections()
        .map_err(|error| format!("Could not inspect connections: {error}"))?;
    let mount = choose_mount(&mounts, &connections)
        .ok_or_else(|| "Create a Notion folder before turning on Live Mode.".to_string())?;
    let now = live_mode_timestamp();
    let existing = store
        .get_mount_live_mode(&mount.mount_id)
        .map_err(|error| format!("Could not inspect Live Mode state: {error}"))?;
    let record = existing.unwrap_or_else(|| {
        MountLiveModeRecord::new(mount.mount_id.clone(), change.enabled, now.clone())
    });
    let record = if change.enabled {
        record.active(None, now.clone(), now)
    } else {
        record.off(now)
    };
    match save_mount_live_mode_and_publish_signal(&mut store, &state_root, record) {
        Ok(_) => wake_live_mode_runner(),
        Err(MountLiveModeStateChangeError::Save(error)) => {
            return Err(format!("Could not update Live Mode state: {error}"));
        }
        Err(MountLiveModeStateChangeError::PublishSignal(error)) => {
            wake_live_mode_runner();
            log_live_mode_state_signal_error(&error);
            return Err(format!(
                "Live Mode state changed, but Locality could not publish its wake signal: {error}"
            ));
        }
    }

    Ok(ActionReport {
        ok: true,
        message: if change.enabled {
            "Live Mode is on for this folder.".to_string()
        } else {
            "Live Mode is off for this folder.".to_string()
        },
    })
}

fn mark_mount_live_mode_syncing(state_root: &Path, mount_id: &MountId) -> Result<bool, String> {
    let mut store = SqliteStateStore::open(state_root.to_path_buf())
        .map_err(|error| format!("Live Mode could not open Locality state: {error}"))?;
    let now = live_mode_timestamp();
    let Some(record) = store
        .get_mount_live_mode(mount_id)
        .map_err(|error| format!("Live Mode could not inspect its state: {error}"))?
    else {
        return Ok(false);
    };
    if !record.enabled {
        return Ok(false);
    }
    let record = record.syncing(now);
    store
        .save_mount_live_mode(record)
        .map_err(|error| format!("Live Mode could not update its state: {error}"))?;
    Ok(true)
}

fn record_mount_live_mode_tick_result(
    state_root: &Path,
    mount_id: &MountId,
    report: &ActionReport,
) -> Result<(), String> {
    let mut store = SqliteStateStore::open(state_root.to_path_buf())
        .map_err(|error| format!("Live Mode could not open Locality state: {error}"))?;
    let now = live_mode_timestamp();
    let Some(record) = store
        .get_mount_live_mode(mount_id)
        .map_err(|error| format!("Live Mode could not inspect its state: {error}"))?
    else {
        return Ok(());
    };
    if !record.enabled {
        return Ok(());
    }
    let record = if report.ok {
        record.active(non_empty_string(report.message.clone()), now.clone(), now)
    } else if live_mode_failure_should_pause(&report.message) {
        record.error(report.message.clone(), now.clone(), now)
    } else {
        record.active(non_empty_string(report.message.clone()), now.clone(), now)
    };
    store
        .save_mount_live_mode(record)
        .map_err(|error| format!("Live Mode could not update its state: {error}"))
}

fn live_mode_failure_should_pause(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    message.starts_with("Live Mode paused for")
        || message.contains("Review required before pushing")
        || lower.contains("could not identify the remote page")
}

fn live_mode_timestamp() -> String {
    auto_save_timestamp()
}

fn non_empty_string(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn live_mode_next_remote_pull_targets_for_state_root(
    state_root: &Path,
    mount: &MountConfig,
    limit: usize,
) -> Result<Vec<LiveModeRemoteTarget>, String> {
    let store = SqliteStateStore::open(state_root.to_path_buf())
        .map_err(|error| format!("Live Mode could not open Locality state: {error}"))?;
    live_mode_next_remote_pull_targets_for_mount(&store, mount, limit)
}

fn live_mode_next_remote_pull_targets_for_mount<S>(
    store: &S,
    mount: &MountConfig,
    limit: usize,
) -> Result<Vec<LiveModeRemoteTarget>, String>
where
    S: EntityRepository + FreshnessStateRepository,
{
    if limit == 0 {
        return Ok(Vec::new());
    }
    let candidates = live_mode_remote_pull_candidates(store, mount)?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let mut cursor = live_mode_remote_pull_cursor()
        .lock()
        .map_err(|_| "Live Mode remote pull cursor is unavailable.".to_string())?;
    let start = *cursor % candidates.len();
    let selected_count = limit.min(candidates.len());
    let selected = (0..selected_count)
        .map(|offset| candidates[(start + offset) % candidates.len()].clone())
        .collect::<Vec<_>>();
    *cursor = cursor.wrapping_add(selected_count);
    Ok(selected)
}

fn live_mode_remote_pull_cursor() -> &'static Mutex<usize> {
    LIVE_MODE_REMOTE_PULL_CURSOR.get_or_init(|| Mutex::new(0))
}

fn live_mode_remote_check_page_budget() -> usize {
    live_mode_remote_check_page_budget_for_rate(
        notion_requests_per_second_setting(),
        LIVE_MODE_ACTIVE_REMOTE_CHECK_INTERVAL,
    )
}

fn live_mode_remote_check_page_budget_for_rate(
    requests_per_second: f64,
    interval: Duration,
) -> usize {
    if !requests_per_second.is_finite()
        || requests_per_second <= 0.0
        || interval.is_zero()
        || !LIVE_MODE_REMOTE_CHECK_ESTIMATED_REQUESTS_PER_PAGE.is_finite()
        || LIVE_MODE_REMOTE_CHECK_ESTIMATED_REQUESTS_PER_PAGE <= 0.0
    {
        return 1;
    }

    (((requests_per_second * interval.as_secs_f64() * LIVE_MODE_REMOTE_CHECK_QUEUE_SHARE)
        / LIVE_MODE_REMOTE_CHECK_ESTIMATED_REQUESTS_PER_PAGE)
        .floor()
        .max(1.0) as usize)
        .min(LIVE_MODE_REMOTE_CHECK_MAX_BATCH_PAGES)
}

fn live_mode_remote_pull_scan_is_due(mount: &MountConfig) -> bool {
    let Ok(mut times) = live_mode_remote_pull_scan_times().lock() else {
        return true;
    };
    live_mode_remote_pull_scan_is_due_for_key(
        &mut times,
        mount.mount_id.clone(),
        Instant::now(),
        LIVE_MODE_ACTIVE_REMOTE_CHECK_INTERVAL,
    )
}

fn live_mode_remote_pull_scan_times() -> &'static Mutex<BTreeMap<MountId, Instant>> {
    LIVE_MODE_REMOTE_PULL_SCAN_TIMES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn live_mode_remote_pull_scan_is_due_for_key(
    times: &mut BTreeMap<MountId, Instant>,
    key: MountId,
    now: Instant,
    interval: Duration,
) -> bool {
    if times
        .get(&key)
        .and_then(|last| now.checked_duration_since(*last))
        .is_some_and(|elapsed| elapsed < interval)
    {
        return false;
    }
    times.insert(key, now);
    true
}

fn live_mode_remote_pull_candidates<S>(
    store: &S,
    mount: &MountConfig,
) -> Result<Vec<LiveModeRemoteTarget>, String>
where
    S: EntityRepository + FreshnessStateRepository,
{
    let access_root = mount_access_root(mount);
    let now_ms = live_mode_now_ms();
    let freshness_by_remote_id = store
        .list_freshness_states(&mount.mount_id)
        .map_err(|error| format!("Live Mode could not inspect file activity: {error}"))?
        .into_iter()
        .map(|state| (state.remote_id.clone(), state))
        .collect::<BTreeMap<_, _>>();
    let mut candidates = store
        .list_entities(&mount.mount_id)
        .map_err(|error| format!("Live Mode could not inspect mounted pages: {error}"))?
        .into_iter()
        .filter(|entity| {
            entity.kind == EntityKind::Page && entity.hydration == HydrationState::Hydrated
        })
        .filter(|entity| {
            let freshness = freshness_by_remote_id.get(&entity.remote_id);
            live_mode_target_is_recently_active(freshness, now_ms)
                && (freshness.is_some_and(|freshness| freshness.remote_hint_pending)
                    || live_mode_remote_check_is_due(freshness, now_ms))
        })
        .map(|entity| LiveModeRemoteTarget {
            mount_id: mount.mount_id.clone(),
            remote_id: entity.remote_id,
            path: access_root.join(entity.path),
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.remote_id.0.cmp(&right.remote_id.0))
    });
    Ok(candidates)
}

fn live_mode_reconcile_recent_local_targets(
    state_root: &Path,
    mount: &MountConfig,
) -> Result<(), String> {
    let mut store = SqliteStateStore::open(state_root.to_path_buf())
        .map_err(|error| format!("Live Mode could not open Locality state: {error}"))?;
    let targets = live_mode_local_reconcile_targets_for_mount(
        &store,
        mount,
        live_mode_remote_check_page_budget(),
    )?;
    for target in targets {
        if !live_mode_should_reconcile_local_target(&target) {
            continue;
        }
        let report = live_mode_reconcile_local_target_with_store(&mut store, state_root, &target)?;
        if report.reconciled > 0 {
            desktop_log(
                "info",
                "live_mode.local_projection_reconciled",
                format!(
                    "imported {} visible File Provider edit(s) before Live Mode sync for `{}`",
                    report.reconciled,
                    target.path.display()
                ),
            );
        }
    }
    Ok(())
}

fn live_mode_local_reconcile_targets_for_mount<S>(
    store: &S,
    mount: &MountConfig,
    limit: usize,
) -> Result<Vec<LiveModeRemoteTarget>, String>
where
    S: EntityRepository + FreshnessStateRepository,
{
    live_mode_local_reconcile_targets_for_mount_at(store, mount, limit, live_mode_now_ms())
}

fn live_mode_local_reconcile_targets_for_mount_at<S>(
    store: &S,
    mount: &MountConfig,
    limit: usize,
    now_ms: u128,
) -> Result<Vec<LiveModeRemoteTarget>, String>
where
    S: EntityRepository + FreshnessStateRepository,
{
    if limit == 0 {
        return Ok(Vec::new());
    }

    let access_root = mount_access_root(mount);
    let freshness_by_remote_id = store
        .list_freshness_states(&mount.mount_id)
        .map_err(|error| format!("Live Mode could not inspect file activity: {error}"))?
        .into_iter()
        .map(|state| (state.remote_id.clone(), state))
        .collect::<BTreeMap<_, _>>();
    let mut candidates = store
        .list_entities(&mount.mount_id)
        .map_err(|error| format!("Live Mode could not inspect mounted pages: {error}"))?
        .into_iter()
        .filter(|entity| {
            entity.kind == EntityKind::Page && entity.hydration == HydrationState::Hydrated
        })
        .filter_map(|entity| {
            let freshness = freshness_by_remote_id.get(&entity.remote_id);
            if !live_mode_target_is_recently_active(freshness, now_ms) {
                return None;
            }
            let activity = live_mode_local_activity_sort_value(freshness)?;
            Some((
                activity,
                entity.path.clone(),
                LiveModeRemoteTarget {
                    mount_id: mount.mount_id.clone(),
                    remote_id: entity.remote_id,
                    path: access_root.join(entity.path),
                },
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    candidates.truncate(limit);
    Ok(candidates
        .into_iter()
        .map(|(_, _, target)| target)
        .collect())
}

fn live_mode_local_activity_sort_value(freshness: Option<&FreshnessStateRecord>) -> Option<u128> {
    let freshness = freshness?;
    [
        freshness.last_local_change_at.as_deref(),
        freshness.last_opened_at.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(timestamp_sort_value)
    .max()
}

fn live_mode_target_is_recently_active(
    freshness: Option<&FreshnessStateRecord>,
    now_ms: u128,
) -> bool {
    let Some(freshness) = freshness else {
        return false;
    };
    freshness.remote_hint_pending
        || live_mode_timestamp_is_within(
            freshness.last_opened_at.as_deref(),
            now_ms,
            LIVE_MODE_ACTIVE_TARGET_WINDOW,
        )
        || live_mode_timestamp_is_within(
            freshness.last_local_change_at.as_deref(),
            now_ms,
            LIVE_MODE_ACTIVE_TARGET_WINDOW,
        )
}

fn live_mode_remote_check_is_due(freshness: Option<&FreshnessStateRecord>, now_ms: u128) -> bool {
    let Some(freshness) = freshness else {
        return true;
    };
    if freshness.remote_hint_pending {
        return false;
    }
    !live_mode_timestamp_is_within(
        freshness.last_checked_at.as_deref(),
        now_ms,
        LIVE_MODE_ACTIVE_REMOTE_CHECK_INTERVAL,
    )
}

fn live_mode_timestamp_is_within(
    timestamp: Option<&str>,
    now_ms: u128,
    interval: Duration,
) -> bool {
    let Some(timestamp_ms) = timestamp.and_then(timestamp_sort_value) else {
        return false;
    };
    now_ms.saturating_sub(timestamp_ms) <= interval.as_millis()
}

fn live_mode_now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn live_mode_reconcile_local_target(
    target: &LiveModeRemoteTarget,
) -> Result<daemon_file_provider::ProjectionReconcileReport, String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Live Mode could not open Locality state: {error}"))?;
    live_mode_reconcile_local_target_with_store(&mut store, &state_root, target)
}

fn live_mode_reconcile_local_target_with_store(
    store: &mut SqliteStateStore,
    state_root: &Path,
    target: &LiveModeRemoteTarget,
) -> Result<daemon_file_provider::ProjectionReconcileReport, String> {
    daemon_file_provider::reconcile_newer_macos_file_provider_projection(
        store,
        state_root,
        Some(&target.path),
    )
    .map_err(|error| format!("Live Mode could not inspect local File Provider edits: {error}"))
}

fn live_mode_should_reconcile_local_target(target: &LiveModeRemoteTarget) -> bool {
    let Ok(mut times) = live_mode_local_reconcile_times().lock() else {
        return true;
    };
    live_mode_should_reconcile_local_target_for_key(
        &mut times,
        target.path.clone(),
        Instant::now(),
        LIVE_MODE_LOCAL_RECONCILE_INTERVAL,
    )
}

fn live_mode_local_reconcile_times() -> &'static Mutex<BTreeMap<PathBuf, Instant>> {
    LIVE_MODE_LOCAL_RECONCILE_TIMES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn live_mode_should_reconcile_local_target_for_key(
    times: &mut BTreeMap<PathBuf, Instant>,
    key: PathBuf,
    now: Instant,
    interval: Duration,
) -> bool {
    if times
        .get(&key)
        .and_then(|last| now.checked_duration_since(*last))
        .is_some_and(|elapsed| elapsed < interval)
    {
        return false;
    }

    times.insert(key, now);
    true
}

fn live_mode_queue_remote_fast_forward_at_state_root(
    state_root: &Path,
    target: &LiveModeRemoteTarget,
) -> Result<(), String> {
    let key = (target.mount_id.clone(), target.remote_id.clone());
    if !live_mode_claim_remote_fast_forward_key(&key, Instant::now()) {
        return Ok(());
    }

    let response = send_request(
        state_root,
        &DaemonRequest::RemoteFastForward {
            mount_id: target.mount_id.0.clone(),
            remote_id: target.remote_id.0.clone(),
            path: target.path.clone(),
        },
    )
    .map_err(|error| {
        live_mode_release_remote_fast_forward_key(&key);
        format!(
            "Live Mode could not reach the Locality daemon to queue remote sync work: {}",
            error.message()
        )
    })?;
    if !response.ok {
        live_mode_release_remote_fast_forward_key(&key);
        let message = response
            .error
            .map(|error| error.message)
            .unwrap_or_else(|| "daemon rejected the Live Mode request".to_string());
        return Err(format!(
            "Live Mode could not queue remote sync work: {message}"
        ));
    }

    if response
        .payload
        .as_ref()
        .and_then(|payload| payload.get("queued"))
        .and_then(|queued| queued.as_bool())
        .unwrap_or(true)
    {
        desktop_log(
            "info",
            "live_mode_remote_fast_forward_queued",
            format!(
                "queued remote fast-forward for `{}` ({}/{})",
                target.path.display(),
                target.mount_id.as_str(),
                target.remote_id.as_str()
            ),
        );
    }
    Ok(())
}

fn live_mode_claim_remote_fast_forward_key(key: &(MountId, RemoteId), now: Instant) -> bool {
    let Ok(mut times) = live_mode_remote_fast_forward_times().lock() else {
        return true;
    };
    times.retain(|_, last| {
        now.checked_duration_since(*last)
            .is_some_and(|elapsed| elapsed < LIVE_MODE_REMOTE_FAST_FORWARD_LEASE)
    });
    if times.contains_key(key) {
        return false;
    }
    times.insert(key.clone(), now);
    true
}

fn live_mode_release_remote_fast_forward_key(key: &(MountId, RemoteId)) {
    let Ok(mut times) = live_mode_remote_fast_forward_times().lock() else {
        return;
    };
    times.remove(key);
}

fn live_mode_remote_fast_forward_times() -> &'static Mutex<BTreeMap<(MountId, RemoteId), Instant>> {
    LIVE_MODE_REMOTE_FAST_FORWARD_TIMES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveModeRemoteDriftMerge {
    Unchanged,
    Clean,
    ConflictMarkersWritten,
}

fn live_mode_merge_remote_drift_target(
    _change: &PendingChange,
    target: &Path,
) -> Result<LiveModeRemoteDriftMerge, String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Live Mode could not open Locality state: {error}"))?;
    reconcile_desktop_projection_changes(&mut store, &state_root, Some(target))?;
    let target = absolute_path(target)?;
    let (mount, relative_path) = resolve_desktop_mount_path(&store, &target)?;
    let Some(mut entity) = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(|error| format!("Live Mode could not inspect local metadata: {error}"))?
    else {
        return Ok(LiveModeRemoteDriftMerge::Unchanged);
    };
    if entity.kind != EntityKind::Page {
        return Ok(LiveModeRemoteDriftMerge::Unchanged);
    }
    let previous_shadow = match store.load_shadow(&mount.mount_id, &entity.remote_id) {
        Ok(shadow) => shadow,
        Err(locality_store::StoreError::ShadowMissing { .. }) => {
            return Ok(LiveModeRemoteDriftMerge::Unchanged);
        }
        Err(error) => {
            return Err(format!(
                "Live Mode could not inspect the current page shadow: {error}"
            ));
        }
    };

    let credentials = open_credential_store(&state_root);
    let connector = resolve_source_for_mount_id(&store, credentials.as_ref(), &mount.mount_id)
        .map_err(|error| error.message())?;
    let rendered = connector
        .fetch_render(&HydrationRequest::new(
            mount.mount_id.clone(),
            entity.remote_id.clone(),
            entity.path.clone(),
            HydrationState::Hydrated,
            HydrationReason::RemoteFastForward,
        ))
        .map_err(|error| format!("Live Mode could not inspect Notion changes: {error}"))?;
    if rendered.shadow == previous_shadow {
        return Ok(LiveModeRemoteDriftMerge::Unchanged);
    }

    let output_root = live_mode_projection_output_root(&state_root, &mount);
    for asset in &rendered.assets {
        let path = safe_join_relative(&output_root, &asset.path)?;
        write_file_atomic(&path, &asset.bytes).map_err(|error| {
            format!(
                "Live Mode could not write media asset `{}`: {error}",
                path.display()
            )
        })?;
    }
    update_hydrated_media_manifest(&output_root, &rendered.assets)
        .map_err(|error| format!("Live Mode could not update media metadata: {error}"))?;

    let read_path = live_mode_projection_read_path(&state_root, &mount, &relative_path, &target)?;
    let local_contents = fs::read_to_string(&read_path).map_err(|error| {
        format!(
            "Live Mode could not read `{}`: {error}",
            read_path.display()
        )
    })?;
    let remote_document =
        document_with_absolute_media_hrefs(&rendered.document, &entity.path, &output_root);
    let merge = live_mode_merge_remote_drift_markdown(
        &local_contents,
        previous_shadow.rendered_body.as_str(),
        &remote_document,
    );

    write_file_atomic(&read_path, merge.markdown.as_bytes()).map_err(|error| {
        format!(
            "Live Mode could not write `{}`: {error}",
            read_path.display()
        )
    })?;
    if read_path != target && target.exists() {
        write_file_atomic(&target, merge.markdown.as_bytes()).map_err(|error| {
            format!(
                "Live Mode could not refresh visible file `{}`: {error}",
                target.display()
            )
        })?;
    }

    store
        .save_shadow(&mount.mount_id, rendered.shadow.clone())
        .map_err(|error| format!("Live Mode could not update local shadow: {error}"))?;
    if entity.hydration.can_transition_to(&HydrationState::Dirty) {
        entity.hydration = if merge.has_conflicts {
            HydrationState::Conflicted
        } else {
            HydrationState::Dirty
        };
    }
    entity.content_hash = Some(rendered.shadow.body_hash.clone());
    if rendered.remote_edited_at.is_some() {
        entity.remote_edited_at = rendered.remote_edited_at.clone();
    }
    store
        .save_entity(entity.clone())
        .map_err(|error| format!("Live Mode could not update local metadata: {error}"))?;
    save_live_mode_remote_observation(&mut store, &mount, &entity, rendered.remote_edited_at)?;
    clear_live_mode_remote_hint(&mut store, &mount.mount_id, &entity.remote_id)?;

    if merge.has_conflicts {
        Ok(LiveModeRemoteDriftMerge::ConflictMarkersWritten)
    } else {
        Ok(LiveModeRemoteDriftMerge::Clean)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LiveModeRemoteDriftMarkdownMerge {
    markdown: String,
    has_conflicts: bool,
}

fn live_mode_merge_remote_drift_markdown(
    local_contents: &str,
    base_body: &str,
    remote_document: &locality_core::model::CanonicalDocument,
) -> LiveModeRemoteDriftMarkdownMerge {
    let merged =
        render_inline_conflict_markdown_with_base(local_contents, Some(base_body), remote_document);
    LiveModeRemoteDriftMarkdownMerge {
        has_conflicts: has_unresolved_conflict_markers(&merged),
        markdown: merged,
    }
}

fn live_mode_projection_output_root(state_root: &Path, mount: &MountConfig) -> PathBuf {
    if mount.projection.uses_virtual_filesystem() {
        virtual_fs_content_root(state_root, &mount.mount_id)
    } else {
        mount.root.clone()
    }
}

fn live_mode_projection_read_path(
    state_root: &Path,
    mount: &MountConfig,
    relative_path: &Path,
    target: &Path,
) -> Result<PathBuf, String> {
    if mount.projection.uses_virtual_filesystem() {
        virtual_fs_content_path(state_root, &mount.mount_id, relative_path)
            .map_err(|error| error.to_string())
    } else {
        Ok(target.to_path_buf())
    }
}

fn safe_join_relative(root: &Path, relative_path: &Path) -> Result<PathBuf, String> {
    if relative_path.components().any(|component| {
        matches!(
            component,
            std::path::Component::Prefix(_)
                | std::path::Component::RootDir
                | std::path::Component::ParentDir
        )
    }) {
        return Err(format!(
            "Live Mode received an unsafe media path `{}`.",
            relative_path.display()
        ));
    }
    Ok(root.join(relative_path))
}

fn save_live_mode_remote_observation(
    store: &mut SqliteStateStore,
    mount: &MountConfig,
    entity: &EntityRecord,
    remote_edited_at: Option<String>,
) -> Result<(), String> {
    let mut observation = RemoteObservationRecord::new(
        mount.mount_id.clone(),
        entity.remote_id.clone(),
        entity.kind.clone(),
        entity.title.clone(),
        entity.path.clone(),
        activity_timestamp(),
    );
    if let Some(remote_edited_at) = remote_edited_at {
        observation = observation.with_remote_version(RemoteVersion::new(remote_edited_at));
    }
    store
        .save_remote_observation(observation)
        .map_err(|error| format!("Live Mode could not update remote metadata: {error}"))
}

fn clear_live_mode_remote_hint(
    store: &mut SqliteStateStore,
    mount_id: &MountId,
    remote_id: &RemoteId,
) -> Result<(), String> {
    let Some(mut freshness) = store
        .get_freshness_state(mount_id, remote_id)
        .map_err(|error| format!("Live Mode could not update freshness metadata: {error}"))?
    else {
        return Ok(());
    };
    freshness.remote_hint_pending = false;
    store
        .save_freshness_state(freshness)
        .map_err(|error| format!("Live Mode could not update freshness metadata: {error}"))
}

#[tauri::command]
async fn open_path(path: String) -> ActionReport {
    match tauri::async_runtime::spawn_blocking(move || {
        let expanded = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
        open_virtual_mount_or_path(&expanded).map(|()| expanded)
    })
    .await
    .map_err(|error| format!("Open worker failed: {error}"))
    .and_then(|result| result)
    {
        Ok(expanded) => ActionReport {
            ok: true,
            message: format!("Opened {}", expanded.display()),
        },
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
async fn open_logs_folder() -> ActionReport {
    match tauri::async_runtime::spawn_blocking(move || {
        let path = platform_logs_dir(&default_state_root());
        fs::create_dir_all(&path).map_err(|error| {
            format!("Could not create logs folder `{}`: {error}", path.display())
        })?;
        open_in_file_manager(&path).map(|()| path)
    })
    .await
    .map_err(|error| format!("Open logs worker failed: {error}"))
    .and_then(|result| result)
    {
        Ok(path) => ActionReport {
            ok: true,
            message: format!("Opened logs folder {}", path.display()),
        },
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
async fn open_in_vs_code(path: String) -> ActionReport {
    match tauri::async_runtime::spawn_blocking(move || {
        let expanded = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
        open_path_in_vs_code(&expanded).map(|()| expanded)
    })
    .await
    .map_err(|error| format!("VS Code worker failed: {error}"))
    .and_then(|result| result)
    {
        Ok(expanded) => ActionReport {
            ok: true,
            message: format!("Opened {} in VS Code", expanded.display()),
        },
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
async fn open_mount_folder(request: MountIdRequest) -> ActionReport {
    match tauri::async_runtime::spawn_blocking(move || {
        let state_root = default_state_root();
        let store = SqliteStateStore::open(state_root)
            .map_err(|error| format!("Could not open Locality state: {error}"))?;
        let mount = desktop_mount_by_id(&store, &request.mount_id)?;
        let path = mount_access_root(&mount);
        open_virtual_mount_or_path(&path).map(|()| mount)
    })
    .await
    .map_err(|error| format!("Open mount worker failed: {error}"))
    .and_then(|result| result)
    {
        Ok(mount) => ActionReport {
            ok: true,
            message: format!("Opened mount `{}`.", mount.mount_id.0),
        },
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
async fn open_mount_in_vs_code(request: MountIdRequest) -> ActionReport {
    match tauri::async_runtime::spawn_blocking(move || {
        let state_root = default_state_root();
        let store = SqliteStateStore::open(state_root)
            .map_err(|error| format!("Could not open Locality state: {error}"))?;
        let mount = desktop_mount_by_id(&store, &request.mount_id)?;
        let path = mount_access_root(&mount);
        open_path_in_vs_code(&path).map(|()| mount)
    })
    .await
    .map_err(|error| format!("VS Code mount worker failed: {error}"))
    .and_then(|result| result)
    {
        Ok(mount) => ActionReport {
            ok: true,
            message: format!("Opened mount `{}` in VS Code.", mount.mount_id.0),
        },
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
async fn reveal_path(path: String) -> ActionReport {
    match tauri::async_runtime::spawn_blocking(move || {
        let expanded = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
        reveal_virtual_mount_or_path(&expanded).map(|()| expanded)
    })
    .await
    .map_err(|error| format!("Reveal worker failed: {error}"))
    .and_then(|result| result)
    {
        Ok(expanded) => ActionReport {
            ok: true,
            message: format!("Revealed {}", expanded.display()),
        },
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
fn show_main_window(app: AppHandle, view: Option<String>) -> ActionReport {
    show_main_window_with_view(&app, view.as_deref());
    ActionReport {
        ok: true,
        message: "Opened Locality.".to_string(),
    }
}

#[tauri::command]
fn hide_menubar(app: AppHandle) -> ActionReport {
    set_menu_bar_visible(&app, false).unwrap_or_else(action_error)
}

#[tauri::command]
async fn set_desktop_setting(app: AppHandle, change: DesktopSettingChange) -> ActionReport {
    match change.key.as_str() {
        "launch_at_login" => {
            match tauri::async_runtime::spawn_blocking(move || set_launch_at_login(change.enabled))
                .await
                .map_err(|error| format!("Desktop setting worker failed: {error}"))
                .and_then(|result| result)
            {
                Ok(report) => report,
                Err(message) => action_error(message),
            }
        }
        "show_menu_bar" => set_menu_bar_visible(&app, change.enabled).unwrap_or_else(action_error),
        _ => ActionReport {
            ok: false,
            message: format!("Unknown desktop setting `{}`.", change.key),
        },
    }
}

#[tauri::command]
fn set_live_mode_for_file(change: LiveModeFileChange) -> ActionReport {
    set_live_mode_for_file_blocking(change).unwrap_or_else(action_error)
}

#[tauri::command]
fn set_auto_save_for_file(change: LiveModeFileChange) -> ActionReport {
    set_live_mode_for_file(change)
}

#[tauri::command]
fn quit_completely(app: AppHandle) -> ActionReport {
    stop_windows_cloud_files_provider_supervisor_for_shutdown();
    app.exit(0);
    ActionReport {
        ok: true,
        message: "Locality is quitting.".to_string(),
    }
}

#[tauri::command]
async fn schedule_update_relaunch() -> ActionReport {
    match tauri::async_runtime::spawn_blocking(schedule_update_relaunch_blocking).await {
        Ok(Ok(message)) => ActionReport { ok: true, message },
        Ok(Err(message)) => ActionReport { ok: false, message },
        Err(error) => ActionReport {
            ok: false,
            message: format!("Could not schedule relaunch: {error}"),
        },
    }
}

fn schedule_update_relaunch_blocking() -> Result<String, String> {
    let pid = std::process::id();
    let executable = env::current_exe()
        .map_err(|error| format!("Could not resolve Locality executable: {error}"))?;
    schedule_relaunch_after_process_exit(pid, &executable)?;
    Ok("Locality relaunch scheduled.".to_string())
}

fn load_desktop_snapshot() -> Result<DesktopSnapshot, String> {
    let state_root = default_state_root();
    load_desktop_snapshot_at_state_root(&state_root)
}

fn load_desktop_snapshot_at_state_root(state_root: &Path) -> Result<DesktopSnapshot, String> {
    let store =
        SqliteStateStore::open(state_root.to_path_buf()).map_err(|error| error.to_string())?;
    load_desktop_snapshot_from_store(&store, &state_root)
}

fn load_desktop_snapshot_for_surface() -> Result<DesktopSnapshot, String> {
    let mut cache = DESKTOP_SNAPSHOT_CACHE
        .get_or_init(|| Mutex::new(DesktopSnapshotCache::default()))
        .lock()
        .expect("desktop snapshot cache lock poisoned");
    if let (Some(loaded_at), Some(snapshot)) = (cache.loaded_at, cache.snapshot.as_ref())
        && loaded_at.elapsed() < DESKTOP_SNAPSHOT_CACHE_TTL
    {
        return Ok(snapshot.clone());
    }

    let snapshot = load_desktop_snapshot()?;
    cache.loaded_at = Some(Instant::now());
    cache.snapshot = Some(snapshot.clone());
    Ok(snapshot)
}

fn invalidate_desktop_snapshot_cache() {
    if let Some(cache) = DESKTOP_SNAPSHOT_CACHE.get()
        && let Ok(mut cache) = cache.lock()
    {
        cache.loaded_at = None;
        cache.snapshot = None;
    }
}

fn load_desktop_snapshot_from_store(
    store: &SqliteStateStore,
    state_root: &Path,
) -> Result<DesktopSnapshot, String> {
    let mounts = store.load_mounts().map_err(|error| error.to_string())?;
    let connections = store
        .list_connections()
        .map_err(|error| error.to_string())?;
    let journals = store.list_journal().unwrap_or_default();
    let mount = choose_mount(&mounts, &connections);
    let connection = choose_connection(&connections, mount.as_ref());
    let needs_onboarding = desktop_needs_onboarding(connection.as_ref(), mount.as_ref());
    let pending_changes = match mount.as_ref() {
        Some(mount) => pending_changes_for_mount(store, state_root, &mount.mount_id)?,
        None => Vec::new(),
    };
    let active_mount_id = mount.as_ref().map(|mount| mount.mount_id.clone());
    let mount_summaries = mounts
        .iter()
        .map(|candidate| {
            let connection = choose_connection_for_mount(&connections, candidate);
            let provider = provider_runtime_summary(state_root, candidate);
            let pending_change_count = if active_mount_id.as_ref() == Some(&candidate.mount_id) {
                Some(pending_changes.len())
            } else {
                None
            };
            mount_summary_with_pending_change_count(
                Some(store),
                state_root,
                Some(candidate),
                connection.as_ref(),
                provider,
                pending_change_count,
            )
        })
        .collect::<Vec<_>>();
    let provider = mount
        .as_ref()
        .and_then(|mount| provider_runtime_summary(state_root, mount));
    let live_mode = mount_live_mode_summary(store, mount.as_ref(), &pending_changes);
    let recent_files = match mount.as_ref() {
        Some(mount) if desktop_mount_has_current_access(mount, connection.as_ref()) => {
            recent_files_for_mount(store, state_root, mount, 6)?
        }
        None => Vec::new(),
        Some(_) => Vec::new(),
    };
    let daemon_ready = send_request(state_root, &DaemonRequest::Ping)
        .map(|response| response.ok)
        .unwrap_or(false);
    let health_state = health_state(
        &pending_changes,
        connection.as_ref(),
        daemon_ready,
        provider.as_ref(),
    );

    Ok(DesktopSnapshot {
        health: AppHealth {
            state: health_state.to_string(),
            attention_count: pending_changes.len(),
        },
        connection: connection_summary(connection.as_ref()),
        connections: connection_summaries(&connections),
        mount: mount_summary_with_pending_change_count(
            Some(store),
            state_root,
            mount.as_ref(),
            connection.as_ref(),
            provider,
            Some(pending_changes.len()),
        ),
        mounts: mount_summaries,
        active_mount_id: mount.as_ref().map(|mount| mount.mount_id.0.clone()),
        live_mode,
        needs_onboarding,
        settings: desktop_settings(),
        pending_changes,
        recent_files,
        activity: activity_from_journals(&journals, store, state_root),
        suggestions: vec![ConnectorSuggestion {
            connector: "Linear".to_string(),
            description: "Mount issues and projects as local files.".to_string(),
            state: "planned".to_string(),
        }],
    })
}

fn degraded_snapshot(message: String) -> DesktopSnapshot {
    let state_root = default_state_root();
    DesktopSnapshot {
        health: AppHealth {
            state: "runtime_stopped".to_string(),
            attention_count: 0,
        },
        connection: ConnectionSummary {
            connector: "notion".to_string(),
            workspace_name: "Locality state unavailable".to_string(),
            account_label: "Open Settings to repair".to_string(),
            status: "error".to_string(),
        },
        connections: vec![ConnectionSummary {
            connector: "notion".to_string(),
            workspace_name: "Locality state unavailable".to_string(),
            account_label: "Open Settings to repair".to_string(),
            status: "error".to_string(),
        }],
        mount: MountSummary {
            mount_id: String::new(),
            connector: "notion".to_string(),
            connector_name: "Notion".to_string(),
            connection_id: None,
            workspace_name: "Locality state unavailable".to_string(),
            local_path: absolute_display_path(&default_notion_access_root()),
            notion_url: None,
            access_scope: "State load failed".to_string(),
            remote_root_id: None,
            projection: projection_label(&desktop_projection_mode()).to_string(),
            read_only: false,
            status: "error".to_string(),
            root_exists: false,
            entity_count: 0,
            pending_change_count: 0,
            provider: None,
        },
        mounts: vec![mount_summary(None, &state_root, None, None, None)],
        active_mount_id: None,
        live_mode: MountLiveModeSummary::off(),
        needs_onboarding: false,
        settings: desktop_settings(),
        pending_changes: Vec::new(),
        recent_files: Vec::new(),
        activity: vec![ActivityItem {
            title: "Could not load Locality state".to_string(),
            detail: message,
            when: "Now".to_string(),
            occurred_at: Some(activity_timestamp()),
            kind: "error".to_string(),
        }],
        suggestions: vec![ConnectorSuggestion {
            connector: "Linear".to_string(),
            description: "Mount issues and projects as local files.".to_string(),
            state: "planned".to_string(),
        }],
    }
}

fn choose_mount(mounts: &[MountConfig], connections: &[ConnectionRecord]) -> Option<MountConfig> {
    let has_active_connection = |mount: &&MountConfig| {
        choose_connection_for_mount(connections, mount)
            .as_ref()
            .is_some_and(|connection| {
                connection.status == "active" && connection.connector == mount.connector
            })
    };

    mounts
        .iter()
        .find(|mount| mount.connector == "notion" && has_active_connection(mount))
        .or_else(|| mounts.iter().find(has_active_connection))
        .or_else(|| mounts.iter().find(|mount| mount.connector == "notion"))
        .or_else(|| mounts.first())
        .cloned()
}

fn desktop_needs_onboarding(
    connection: Option<&ConnectionRecord>,
    mount: Option<&MountConfig>,
) -> bool {
    !matches!(connection, Some(connection) if connection.status == "active") || mount.is_none()
}

fn desktop_mount_has_current_access(
    mount: &MountConfig,
    connection: Option<&ConnectionRecord>,
) -> bool {
    let Some(connection_id) = mount.connection_id.as_ref() else {
        return true;
    };

    matches!(
        connection,
        Some(connection)
            if connection.status == "active"
                && connection.connector == mount.connector
                && connection.connection_id == *connection_id
    )
}

fn choose_connection(
    connections: &[ConnectionRecord],
    mount: Option<&MountConfig>,
) -> Option<ConnectionRecord> {
    if let Some(connection_id) = mount.and_then(|mount| mount.connection_id.as_ref())
        && let Some(connection) = connections
            .iter()
            .find(|connection| connection.connection_id == *connection_id)
    {
        return Some(connection.clone());
    }

    connections
        .iter()
        .find(|connection| connection.connector == "notion")
        .or_else(|| connections.first())
        .cloned()
}

fn choose_connection_for_mount(
    connections: &[ConnectionRecord],
    mount: &MountConfig,
) -> Option<ConnectionRecord> {
    if let Some(connection_id) = mount.connection_id.as_ref()
        && let Some(connection) = connections
            .iter()
            .find(|connection| connection.connection_id == *connection_id)
    {
        return Some(connection.clone());
    }

    connections
        .iter()
        .find(|connection| connection.connector == mount.connector)
        .cloned()
}

fn connection_summary(connection: Option<&ConnectionRecord>) -> ConnectionSummary {
    let Some(connection) = connection else {
        return ConnectionSummary {
            connector: "notion".to_string(),
            workspace_name: "Notion not connected".to_string(),
            account_label: "Connect a workspace".to_string(),
            status: "missing".to_string(),
        };
    };

    ConnectionSummary {
        connector: connection.connector.clone(),
        workspace_name: connection
            .workspace_name
            .clone()
            .unwrap_or_else(|| connection.display_name.clone()),
        account_label: connection.account_label.clone().unwrap_or_default(),
        status: connection.status.clone(),
    }
}

fn connection_summaries(connections: &[ConnectionRecord]) -> Vec<ConnectionSummary> {
    let mut summaries = connections
        .iter()
        .map(|connection| connection_summary(Some(connection)))
        .collect::<Vec<_>>();
    summaries.sort_by(|left, right| {
        connection_connector_rank(&left.connector)
            .cmp(&connection_connector_rank(&right.connector))
            .then_with(|| left.connector.cmp(&right.connector))
            .then_with(|| left.workspace_name.cmp(&right.workspace_name))
    });
    summaries
}

fn connection_connector_rank(connector: &str) -> usize {
    match connector {
        "notion" => 0,
        "google-docs" => 1,
        "gmail" => 2,
        "granola" => 3,
        _ => 10,
    }
}

fn mount_summary(
    store: Option<&SqliteStateStore>,
    state_root: &Path,
    mount: Option<&MountConfig>,
    connection: Option<&ConnectionRecord>,
    provider: Option<ProviderRuntimeSummary>,
) -> MountSummary {
    mount_summary_with_pending_change_count(store, state_root, mount, connection, provider, None)
}

fn mount_summary_with_pending_change_count(
    store: Option<&SqliteStateStore>,
    state_root: &Path,
    mount: Option<&MountConfig>,
    connection: Option<&ConnectionRecord>,
    provider: Option<ProviderRuntimeSummary>,
    pending_change_count: Option<usize>,
) -> MountSummary {
    let Some(mount) = mount else {
        return MountSummary {
            mount_id: String::new(),
            connector: "notion".to_string(),
            connector_name: "Notion".to_string(),
            connection_id: None,
            workspace_name: connection
                .and_then(|connection| connection.workspace_name.clone())
                .unwrap_or_else(|| "No mounted workspace".to_string()),
            local_path: absolute_display_path(&default_notion_access_root()),
            notion_url: None,
            access_scope: "No mounted access yet".to_string(),
            remote_root_id: None,
            projection: projection_label(&desktop_projection_mode()).to_string(),
            read_only: false,
            status: "not_mounted".to_string(),
            root_exists: false,
            entity_count: 0,
            pending_change_count: 0,
            provider: None,
        };
    };

    let mount_status = provider
        .as_ref()
        .and_then(mount_status_from_provider)
        .unwrap_or("ready");
    let access_root = mount_access_root(mount);
    let root_exists = mount_root_exists_for_desktop_summary(mount, &access_root);

    MountSummary {
        mount_id: mount.mount_id.0.clone(),
        connector: mount.connector.clone(),
        connector_name: connector_label(&mount.connector),
        connection_id: mount
            .connection_id
            .as_ref()
            .map(|connection_id| connection_id.0.clone()),
        workspace_name: connection
            .and_then(|connection| connection.workspace_name.clone())
            .unwrap_or_else(|| connector_label(&mount.connector)),
        local_path: absolute_display_path(&access_root),
        notion_url: mount
            .remote_root_id
            .as_ref()
            .filter(|_| mount.connector == "notion")
            .map(|remote_id| notion_object_url(&remote_id.0)),
        access_scope: mount_access_scope_label(store, mount),
        remote_root_id: mount
            .remote_root_id
            .as_ref()
            .map(|remote_id| remote_id.0.clone()),
        projection: projection_label(&mount.projection).to_string(),
        read_only: mount.read_only,
        status: mount_status.to_string(),
        root_exists,
        entity_count: store
            .and_then(|store| store.list_entities(&mount.mount_id).ok())
            .map(|entities| entities.len())
            .unwrap_or(0),
        pending_change_count: pending_change_count.unwrap_or_else(|| {
            store
                .map(|store| pending_changes_for_mount(store, state_root, &mount.mount_id))
                .and_then(Result::ok)
                .map(|changes| changes.len())
                .unwrap_or(0)
        }),
        provider,
    }
}

fn mount_live_mode_summary(
    store: &SqliteStateStore,
    mount: Option<&MountConfig>,
    pending_changes: &[PendingChange],
) -> MountLiveModeSummary {
    let Some(mount) = mount else {
        return MountLiveModeSummary::off();
    };
    let record = store.get_mount_live_mode(&mount.mount_id).ok().flatten();
    MountLiveModeSummary::from_record(record.as_ref(), pending_changes)
}

impl MountLiveModeSummary {
    fn off() -> Self {
        Self {
            enabled: false,
            state: "off".to_string(),
            label: "Live Mode off".to_string(),
            reason: None,
            last_run_at: None,
            pending_count: 0,
            review_count: 0,
            covered_count: 0,
        }
    }

    fn from_record(
        record: Option<&MountLiveModeRecord>,
        pending_changes: &[PendingChange],
    ) -> Self {
        let pending_count = pending_changes.len();
        let review_count = pending_changes
            .iter()
            .filter(|change| change.state != "safe")
            .count();
        let covered_count = pending_changes
            .iter()
            .filter(|change| change.state == "safe")
            .count();
        let Some(record) = record else {
            return Self {
                pending_count,
                review_count,
                covered_count,
                ..Self::off()
            };
        };
        if !record.enabled && record.state == MountLiveModeState::Error {
            let should_pause = record
                .last_reason
                .as_deref()
                .is_some_and(live_mode_failure_should_pause);
            if pending_changes.is_empty() || !should_pause {
                return Self {
                    pending_count,
                    review_count,
                    covered_count,
                    ..Self::off()
                };
            }
        }
        let state = mount_live_mode_state_label(record);

        Self {
            enabled: record.enabled,
            state: state.0.to_string(),
            label: state.1.to_string(),
            reason: record.last_reason.clone(),
            last_run_at: record.last_run_at.clone(),
            pending_count,
            review_count,
            covered_count,
        }
    }
}

fn mount_live_mode_state_label(record: &MountLiveModeRecord) -> (&'static str, &'static str) {
    if !record.enabled && record.state != MountLiveModeState::Error {
        return ("off", "Live Mode off");
    }

    match record.state {
        MountLiveModeState::Off => ("off", "Live Mode off"),
        MountLiveModeState::Active => ("active", "Live Mode on"),
        MountLiveModeState::Syncing => ("syncing", "Live Mode syncing"),
        MountLiveModeState::Error => ("error", "Live Mode paused"),
    }
}

fn mount_status_from_provider(provider: &ProviderRuntimeSummary) -> Option<&'static str> {
    match provider.state.as_str() {
        "running" if provider.registered == Some(false) => Some("provider_unregistered"),
        "running" => Some("ready"),
        "stopped" => Some("provider_stopped"),
        "error" => Some("provider_error"),
        _ => None,
    }
}

fn provider_runtime_summary(
    state_root: &Path,
    mount: &MountConfig,
) -> Option<ProviderRuntimeSummary> {
    if !mount.projection.uses_virtual_filesystem() {
        return None;
    }
    match mount.projection {
        ProjectionMode::WindowsCloudFiles => {
            Some(windows_cloud_files_provider_status(state_root, mount))
        }
        ProjectionMode::MacosFileProvider
        | ProjectionMode::LinuxFuse
        | ProjectionMode::PlainFiles => None,
    }
}

#[cfg(target_os = "windows")]
fn windows_cloud_files_provider_status(
    state_root: &Path,
    mount: &MountConfig,
) -> ProviderRuntimeSummary {
    match run_windows_cloud_files_lifecycle(
        state_root,
        mount,
        &connector_label(&mount.connector),
        WindowsCloudFilesLifecycleAction::Status,
    ) {
        Ok(report) => {
            let value = report.helper_report;
            ProviderRuntimeSummary {
                state: value
                    .get("state")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("error")
                    .to_string(),
                message: value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Windows Cloud Files provider status is unavailable")
                    .to_string(),
                daemon_running: value
                    .get("daemon_running")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or_else(|| daemon_is_ready(state_root)),
                registered: value.get("registered").and_then(serde_json::Value::as_bool),
                pid: value
                    .get("pid")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|pid| u32::try_from(pid).ok()),
                stale_pid_file: value
                    .get("stale_pid_file")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            }
        }
        Err(error) => ProviderRuntimeSummary {
            state: "error".to_string(),
            message: error.message(),
            daemon_running: daemon_is_ready(state_root),
            registered: None,
            pid: None,
            stale_pid_file: false,
        },
    }
}

#[cfg(not(target_os = "windows"))]
fn windows_cloud_files_provider_status(
    _state_root: &Path,
    mount: &MountConfig,
) -> ProviderRuntimeSummary {
    ProviderRuntimeSummary {
        state: "error".to_string(),
        message: format!(
            "Windows Cloud Files mounts can only be inspected on Windows; mount `{}` cannot be inspected here.",
            mount.mount_id.0
        ),
        daemon_running: false,
        registered: None,
        pid: None,
        stale_pid_file: false,
    }
}

fn pending_changes_for_mount(
    store: &SqliteStateStore,
    state_root: &Path,
    mount_id: &MountId,
) -> Result<Vec<PendingChange>, String> {
    if let Ok(mut repair_store) = SqliteStateStore::open(state_root.to_path_buf()) {
        let _ = repair_clean_remote_deleted_projections(
            &mut repair_store,
            Some(state_root),
            Some(mount_id),
        );
    }
    let status = run_status(
        store,
        StatusOptions {
            state_root: Some(state_root.to_path_buf()),
            mount_id: Some(mount_id.clone()),
            ..StatusOptions::default()
        },
    )
    .map_err(|error| error.message())?;

    Ok(pending_changes_from_status(store, &status))
}

fn pending_changes_from_status<S>(
    store: &S,
    status: &loc_cli::status::StatusReport,
) -> Vec<PendingChange>
where
    S: AutoSaveRepository,
{
    status
        .mounts
        .iter()
        .flat_map(|mount| {
            mount
                .entries
                .iter()
                .filter(|entry| status_entry_needs_desktop_attention(entry))
                .map(move |entry| {
                    let mount_id = MountId::new(mount.mount_id.clone());
                    let live_mode = live_mode_status_for_entry(store, &mount_id, entry);
                    (mount_id, entry, live_mode)
                })
        })
        .map(|(mount_id, entry, live_mode)| PendingChange {
            mount_id: mount_id.0,
            entity_id: entry.entity_id.clone(),
            title: entry.title.clone(),
            local_path: entry.path.clone(),
            summary: status_summary_for_entry(entry),
            state: pending_state_for_entry(entry).to_string(),
            issue_codes: entry
                .issues
                .iter()
                .map(|issue| issue.code.clone())
                .collect(),
            live_mode,
        })
        .collect()
}

fn recent_files_for_mount(
    store: &SqliteStateStore,
    state_root: &Path,
    mount: &MountConfig,
    limit: usize,
) -> Result<Vec<LocatedItem>, String> {
    let status = run_status(
        store,
        StatusOptions {
            state_root: Some(state_root.to_path_buf()),
            mount_id: Some(mount.mount_id.clone()),
            ..StatusOptions::default()
        },
    )
    .map_err(|error| error.message())?;
    let freshness = store
        .list_freshness_states(&mount.mount_id)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|state| (state.remote_id.0.clone(), state))
        .collect::<BTreeMap<_, _>>();

    let mut files = status
        .mounts
        .iter()
        .flat_map(|mount| mount.entries.iter())
        .filter_map(|entry| {
            let freshness = freshness.get(&entry.entity_id);
            let sort_value = recent_file_sort_value(entry, freshness)?;
            Some((
                sort_value,
                entry.title.clone(),
                LocatedItem {
                    title: entry.title.clone(),
                    kind: search_kind_label(&entry.kind).to_string(),
                    local_path: display_path(Path::new(&entry.absolute_path)),
                    state: located_state_for_status_entry(entry).to_string(),
                },
            ))
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    files.truncate(limit);
    Ok(files.into_iter().map(|(_, _, item)| item).collect())
}

fn recent_file_sort_value(
    entry: &loc_cli::status::StatusEntry,
    freshness: Option<&FreshnessStateRecord>,
) -> Option<u128> {
    let freshness_value = freshness
        .into_iter()
        .flat_map(|freshness| {
            [
                freshness.last_local_change_at.as_deref(),
                freshness.last_opened_at.as_deref(),
                freshness.last_checked_at.as_deref(),
            ]
        })
        .flatten()
        .filter_map(timestamp_sort_value)
        .max()
        .unwrap_or(0);

    if freshness_value > 0 {
        return Some(freshness_value);
    }

    if matches!(
        entry.state,
        StatusState::Dirty | StatusState::Conflicted | StatusState::Error | StatusState::Missing
    ) || !matches!(entry.sync_state, StatusSyncState::AllSynced)
    {
        return Some(1);
    }

    None
}

fn timestamp_sort_value(value: &str) -> Option<u128> {
    let value = value.trim();
    if let Some(millis) = value.strip_prefix("unix_ms:") {
        return millis.parse::<u128>().ok();
    }
    value.parse::<u128>().ok()
}

fn located_state_for_status_entry(entry: &loc_cli::status::StatusEntry) -> &'static str {
    if matches!(
        entry.state,
        StatusState::Conflicted | StatusState::Error | StatusState::Missing
    ) || matches!(entry.sync_state, StatusSyncState::Conflicted)
    {
        return "conflict";
    }
    if matches!(entry.state, StatusState::Dirty)
        || matches!(
            entry.sync_state,
            StatusSyncState::PendingLocalChanges | StatusSyncState::ReviewNeeded
        )
    {
        return "pending_changes";
    }
    if matches!(entry.sync_state, StatusSyncState::RemoteUpdateAvailable) {
        return "remote_update_available";
    }
    if matches!(entry.state, StatusState::Stub) {
        return "online_only";
    }
    "ready"
}

fn live_mode_status_for_entry(
    store: &impl AutoSaveRepository,
    mount_id: &MountId,
    entry: &loc_cli::status::StatusEntry,
) -> LiveModeFileStatus {
    live_mode_status_for_path(store, mount_id, Path::new(&entry.path))
}

fn live_mode_status_for_path(
    store: &impl AutoSaveRepository,
    mount_id: &MountId,
    path: &Path,
) -> LiveModeFileStatus {
    let enrollment = store
        .get_auto_save_enrollment(mount_id, path)
        .ok()
        .flatten();
    match enrollment {
        Some(enrollment) if enrollment.enabled => {
            let (state, label) = match enrollment.state {
                AutoSaveState::Active => ("active", "Live Mode on"),
                AutoSaveState::Blocked => ("blocked", "Live Mode blocked"),
                AutoSaveState::PausedRemoteChanged => ("paused_remote_changed", "Live Mode paused"),
                AutoSaveState::PausedFailure => ("paused_failure", "Live Mode paused"),
            };
            LiveModeFileStatus {
                enabled: true,
                state: state.to_string(),
                label: label.to_string(),
                reason: enrollment.last_reason,
            }
        }
        Some(enrollment) => LiveModeFileStatus {
            enabled: false,
            state: "off".to_string(),
            label: "Live Mode off".to_string(),
            reason: enrollment.last_reason,
        },
        None => LiveModeFileStatus {
            enabled: false,
            state: "off".to_string(),
            label: "Live Mode off".to_string(),
            reason: None,
        },
    }
}

fn pending_state_for_entry(entry: &loc_cli::status::StatusEntry) -> &'static str {
    if matches!(entry.sync_state, StatusSyncState::Conflicted) {
        "conflict"
    } else if matches!(entry.state, StatusState::Error | StatusState::Missing)
        || (entry.failed_journal_count > 0 && !failed_journal_only(entry))
    {
        "blocked"
    } else if matches!(
        entry.sync_state,
        StatusSyncState::RemoteUpdateAvailable | StatusSyncState::ReviewNeeded
    ) || entry
        .issues
        .iter()
        .any(|issue| issue.code.contains("large"))
    {
        "needs_review"
    } else {
        "safe"
    }
}

fn status_summary_for_entry(entry: &loc_cli::status::StatusEntry) -> String {
    if entry.failed_journal_count > 0 {
        if let Some(last_failure) = status_issue_message(entry, "last_failure") {
            return failed_push_summary(last_failure);
        }
        return "previous push failed; review this file before trying again".to_string();
    }
    if entry.pending_journal_count > 0 {
        return "push in progress".to_string();
    }
    if matches!(entry.state, StatusState::Conflicted) {
        return "conflict".to_string();
    }
    if status_issue_has_code(entry, "remote_deleted_with_local_pending") {
        return "remote page deleted while local edits are pending".to_string();
    }
    if status_issue_has_code(entry, "remote_deleted") {
        return "remote page deleted or unavailable".to_string();
    }
    if matches!(entry.sync_state, StatusSyncState::RemoteUpdateAvailable) {
        return "remote update available".to_string();
    }
    if matches!(entry.sync_state, StatusSyncState::ReviewNeeded)
        && entry
            .issues
            .iter()
            .any(|issue| issue.code.starts_with("remote_"))
    {
        return "remote changed while local edits are pending".to_string();
    }
    if let Some(issue) = entry.issues.first() {
        return issue.message.clone();
    }
    "local edits pending review".to_string()
}

fn status_issue_has_code(entry: &loc_cli::status::StatusEntry, code: &str) -> bool {
    entry.issues.iter().any(|issue| issue.code == code)
}

fn status_entry_needs_desktop_attention(entry: &loc_cli::status::StatusEntry) -> bool {
    if matches!(
        entry.state,
        StatusState::Dirty | StatusState::Conflicted | StatusState::Missing | StatusState::Error
    ) || matches!(
        entry.sync_state,
        StatusSyncState::RemoteUpdateAvailable
            | StatusSyncState::PendingLocalChanges
            | StatusSyncState::Conflicted
    ) || entry.pending_journal_count > 0
    {
        return true;
    }

    if matches!(entry.sync_state, StatusSyncState::ReviewNeeded) {
        return entry
            .issues
            .iter()
            .any(|issue| !matches!(issue.code.as_str(), "failed_journal" | "last_failure"));
    }

    false
}

fn failed_journal_only(entry: &loc_cli::status::StatusEntry) -> bool {
    entry.failed_journal_count > 0
        && matches!(entry.state, StatusState::Clean)
        && entry.pending_journal_count == 0
        && entry
            .issues
            .iter()
            .all(|issue| matches!(issue.code.as_str(), "failed_journal" | "last_failure"))
}

fn status_issue_message<'a>(
    entry: &'a loc_cli::status::StatusEntry,
    code: &str,
) -> Option<&'a str> {
    entry
        .issues
        .iter()
        .find(|issue| issue.code == code)
        .map(|issue| issue.message.as_str())
}

fn failed_push_summary(message: &str) -> String {
    if is_notion_access_lost_message(message) {
        return notion_access_lost_recovery_message();
    }
    if is_remote_changed_push_message(message) || message.contains("changed since last sync") {
        return "Notion changed since last sync. Pull latest may create conflict markers if the remote and local edits overlap.".to_string();
    }
    if message.contains("unsupported feature") {
        return format!("Previous push hit an unsupported Notion feature: {message}");
    }
    format!("Previous push failed: {message}")
}

fn conflict_preview(contents: &str) -> Option<String> {
    if !has_unresolved_conflict_markers(contents) {
        return None;
    }
    let lines = contents.lines().collect::<Vec<_>>();
    let first_marker = lines
        .iter()
        .position(|line| line.trim_start().starts_with("<<<<<<<"))
        .unwrap_or(0);
    let last_marker = lines
        .iter()
        .enumerate()
        .skip(first_marker)
        .find_map(|(index, line)| line.trim_start().starts_with(">>>>>>>").then_some(index))
        .unwrap_or_else(|| (first_marker + 40).min(lines.len().saturating_sub(1)));
    let start = first_marker.saturating_sub(4);
    let end = (last_marker + 5).min(lines.len());
    let mut preview = lines[start..end].join("\n");
    const MAX_PREVIEW_BYTES: usize = 6000;
    if preview.len() > MAX_PREVIEW_BYTES {
        preview.truncate(MAX_PREVIEW_BYTES);
        preview.push_str("\n...");
    }
    Some(preview)
}

fn activity_from_journals(
    journals: &[JournalEntry],
    store: &SqliteStateStore,
    state_root: &Path,
) -> Vec<ActivityItem> {
    let mut items = load_desktop_activity(state_root).unwrap_or_default();
    let journal_items = journals
        .iter()
        .rev()
        .take(8)
        .map(|journal| {
            let title = journal_title(journal, store);
            let detail = journal_detail(journal);
            ActivityItem {
                title,
                detail,
                when: "Recent".to_string(),
                occurred_at: journal_activity_timestamp(journal),
                kind: "push".to_string(),
            }
        })
        .collect::<Vec<_>>();
    items.extend(journal_items);
    items.truncate(8);

    if items.is_empty() {
        items.push(ActivityItem {
            title: "Locality desktop opened".to_string(),
            detail: "Ready to connect and review workspace changes".to_string(),
            when: "Today".to_string(),
            occurred_at: Some(activity_timestamp()),
            kind: "open".to_string(),
        });
    }

    items
}

fn record_desktop_activity(
    state_root: &Path,
    title: &str,
    detail: &str,
    kind: &str,
) -> Result<(), String> {
    let mut items = load_desktop_activity(state_root).unwrap_or_default();
    items.insert(
        0,
        ActivityItem {
            title: title.to_string(),
            detail: detail.to_string(),
            when: "Recent".to_string(),
            occurred_at: Some(activity_timestamp()),
            kind: kind.to_string(),
        },
    );
    items.truncate(DESKTOP_ACTIVITY_LIMIT);

    let path = desktop_activity_path(state_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create desktop activity folder: {error}"))?;
    }
    let contents = serde_json::to_string_pretty(&items)
        .map_err(|error| format!("Could not serialize desktop activity: {error}"))?;
    fs::write(&path, contents).map_err(|error| format!("Could not write desktop activity: {error}"))
}

fn journal_activity_timestamp(journal: &JournalEntry) -> Option<String> {
    let timestamp = journal.push_id.0.strip_prefix("push-")?.split('-').next()?;
    let nanos = timestamp.parse::<u128>().ok()?;
    Some(format!("unix_ms:{}", nanos / 1_000_000))
}

fn activity_timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("unix_ms:{}", duration.as_millis()),
        Err(_) => "unix_ms:0".to_string(),
    }
}

fn load_desktop_activity(state_root: &Path) -> Result<Vec<ActivityItem>, String> {
    let path = desktop_activity_path(state_root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read desktop activity: {error}"))?;
    serde_json::from_str(&contents)
        .map_err(|error| format!("Could not parse desktop activity: {error}"))
}

fn journal_title(journal: &JournalEntry, store: &SqliteStateStore) -> String {
    let entity_title = journal
        .remote_ids
        .first()
        .and_then(|remote_id| {
            store
                .get_entity(&journal.mount_id, remote_id)
                .ok()
                .flatten()
        })
        .map(|entity| entity.title)
        .unwrap_or_else(|| "Notion content".to_string());
    match journal.status {
        JournalStatus::Failed(_) => format!("Push failed for {entity_title}"),
        JournalStatus::Reverted => format!("Undid push for {entity_title}"),
        _ => format!("Pushed {entity_title} to Notion"),
    }
}

fn journal_detail(journal: &JournalEntry) -> String {
    let operation_count = journal.plan.operations.len();
    match &journal.status {
        JournalStatus::Failed(message) => message.clone(),
        JournalStatus::Prepared => format!("{operation_count} changes prepared"),
        JournalStatus::Applying => format!("{operation_count} changes applying"),
        JournalStatus::Applied | JournalStatus::Reconciled => {
            format!("{operation_count} remote changes")
        }
        JournalStatus::Reverted => format!("{operation_count} changes reverted"),
    }
}

fn health_state(
    pending_changes: &[PendingChange],
    connection: Option<&ConnectionRecord>,
    daemon_ready: bool,
    provider: Option<&ProviderRuntimeSummary>,
) -> &'static str {
    if connection.is_some_and(|connection| connection.status != "active") {
        "reconnect_needed"
    } else if !daemon_ready {
        "stopped"
    } else if provider.is_some_and(provider_needs_repair) {
        "runtime_stopped"
    } else if !pending_changes.is_empty() {
        "needs_review"
    } else {
        "ready"
    }
}

fn refresh_tray_icon(app: &AppHandle) {
    if let Ok(snapshot) = load_desktop_snapshot() {
        refresh_tray_icon_for_snapshot(app, &snapshot);
        return;
    }

    set_tray_icon_and_tooltip(app, TrayVisualState::Reconnect, "Locality needs attention");
}

fn refresh_tray_icon_for_snapshot(app: &AppHandle, snapshot: &DesktopSnapshot) {
    set_tray_icon_and_tooltip(
        app,
        tray_state_for_health(&snapshot.health.state),
        &tray_tooltip(snapshot),
    );
    sync_tray_visibility(app, &snapshot.settings);
}

fn set_tray_icon_and_tooltip(app: &AppHandle, state: TrayVisualState, tooltip: &str) {
    if let Some(tray) = app.tray_by_id("main") {
        let next = TrayRenderState {
            state,
            tooltip: tooltip.to_string(),
        };
        let should_update = {
            let mut last_render = LAST_TRAY_RENDER
                .get_or_init(|| Mutex::new(None))
                .lock()
                .expect("tray render lock poisoned");
            if last_render.as_ref() == Some(&next) {
                false
            } else {
                *last_render = Some(next);
                true
            }
        };
        if !should_update {
            return;
        }

        let _ = tray.set_icon_with_as_template(
            Some(tray_icon_image(state)),
            tray_icon_should_use_template(),
        );
        let _ = tray.set_tooltip(Some(tooltip.to_string()));
    }
}

fn provider_needs_repair(provider: &ProviderRuntimeSummary) -> bool {
    provider.state != "running" || provider.registered == Some(false)
}

fn sync_tray_visibility(app: &AppHandle, settings: &DesktopSettings) {
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_visible(settings.show_menu_bar);
    }
}

fn tray_state_for_health(state: &str) -> TrayVisualState {
    if state == "reconnect_needed" || state == "stopped" || state == "runtime_stopped" {
        TrayVisualState::Reconnect
    } else if state == "needs_review" {
        TrayVisualState::Review
    } else {
        TrayVisualState::Ready
    }
}

fn tray_tooltip(snapshot: &DesktopSnapshot) -> String {
    match snapshot.health.state.as_str() {
        "needs_review" => format!(
            "Locality: {} pending changes",
            snapshot.health.attention_count
        ),
        "checking_freshness" => "Locality: checking freshness".to_string(),
        "reconnect_needed" => "Locality: reconnect Notion".to_string(),
        "stopped" => "Locality: daemon stopped".to_string(),
        "runtime_stopped" => "Locality: provider needs repair".to_string(),
        _ => "Locality: ready".to_string(),
    }
}

fn tray_icon_image(state: TrayVisualState) -> Image<'static> {
    let size = 36;
    let mut rgba = vec![0; size * size * 4];
    if tray_icon_should_use_template() {
        draw_short_logo_mark(&mut rgba, size, [17, 24, 39, 255], false);

        match state {
            TrayVisualState::Ready => {}
            TrayVisualState::Review | TrayVisualState::Reconnect => {
                draw_disc(&mut rgba, size, (27.0, 9.0), 3.8, [17, 24, 39, 255]);
            }
        }
    } else {
        draw_short_logo_mark(&mut rgba, size, [255, 255, 255, 255], true);
        draw_short_logo_mark(&mut rgba, size, [17, 24, 39, 255], false);

        match state {
            TrayVisualState::Ready => {}
            TrayVisualState::Review => {
                draw_disc(&mut rgba, size, (27.0, 9.0), 5.2, [255, 255, 255, 255]);
                draw_disc(&mut rgba, size, (27.0, 9.0), 3.6, [217, 140, 31, 255]);
            }
            TrayVisualState::Reconnect => {
                draw_disc(&mut rgba, size, (27.0, 9.0), 5.2, [255, 255, 255, 255]);
                draw_disc(&mut rgba, size, (27.0, 9.0), 3.6, [207, 63, 63, 255]);
            }
        }
    }

    Image::new_owned(rgba, size as u32, size as u32)
}

fn tray_icon_should_use_template() -> bool {
    cfg!(target_os = "macos")
}

const TRAY_LOGO_MARK_PNG: &[u8] = include_bytes!("../assets/tray-locality-mark.png");

fn draw_short_logo_mark(rgba: &mut [u8], size: usize, color: [u8; 4], halo: bool) {
    let mark = tray_logo_mark_image();
    if halo {
        for y_offset in -1..=1 {
            for x_offset in -1..=1 {
                if x_offset != 0 || y_offset != 0 {
                    draw_image_alpha_mask(rgba, size, mark, x_offset, y_offset, color);
                }
            }
        }
    }
    draw_image_alpha_mask(rgba, size, mark, 0, 0, color);
}

fn tray_logo_mark_image() -> &'static Image<'static> {
    static TRAY_LOGO_MARK: OnceLock<Image<'static>> = OnceLock::new();
    TRAY_LOGO_MARK.get_or_init(|| {
        Image::from_bytes(TRAY_LOGO_MARK_PNG).expect("embedded tray logo PNG should decode")
    })
}

fn draw_image_alpha_mask(
    rgba: &mut [u8],
    size: usize,
    image: &Image<'_>,
    x_offset: i32,
    y_offset: i32,
    color: [u8; 4],
) {
    let image_width = image.width() as usize;
    let image_height = image.height() as usize;
    let image_rgba = image.rgba();
    for y in 0..image_height {
        let target_y = y as i32 + y_offset;
        if !(0..size as i32).contains(&target_y) {
            continue;
        }
        for x in 0..image_width {
            let target_x = x as i32 + x_offset;
            if !(0..size as i32).contains(&target_x) {
                continue;
            }
            let alpha = image_rgba[(y * image_width + x) * 4 + 3] as f64 / 255.0;
            if alpha > 0.0 {
                blend_pixel(
                    rgba,
                    size,
                    target_x as usize,
                    target_y as usize,
                    color,
                    alpha,
                );
            }
        }
    }
}

fn draw_disc(rgba: &mut [u8], size: usize, center: (f64, f64), radius: f64, color: [u8; 4]) {
    for y in 0..size {
        for x in 0..size {
            let dx = x as f64 + 0.5 - center.0;
            let dy = y as f64 + 0.5 - center.1;
            let distance = (dx * dx + dy * dy).sqrt();
            let alpha = (radius + 0.6 - distance).clamp(0.0, 1.0);
            if alpha > 0.0 {
                blend_pixel(rgba, size, x, y, color, alpha);
            }
        }
    }
}

fn blend_pixel(rgba: &mut [u8], size: usize, x: usize, y: usize, color: [u8; 4], coverage: f64) {
    let idx = (y * size + x) * 4;
    let src_alpha = (color[3] as f64 / 255.0) * coverage;
    let dst_alpha = rgba[idx + 3] as f64 / 255.0;
    let out_alpha = src_alpha + dst_alpha * (1.0 - src_alpha);
    if out_alpha <= f64::EPSILON {
        return;
    }

    for channel in 0..3 {
        let src = color[channel] as f64 / 255.0;
        let dst = rgba[idx + channel] as f64 / 255.0;
        let out = (src * src_alpha + dst * dst_alpha * (1.0 - src_alpha)) / out_alpha;
        rgba[idx + channel] = (out * 255.0).round() as u8;
    }
    rgba[idx + 3] = (out_alpha * 255.0).round() as u8;
}

fn locate_notion_query(query: &str) -> Result<LocatedItem, String> {
    if let Some(message) = unsupported_notion_locator_url_message(query) {
        return Err(message);
    }

    if notion_id_from_url(query).is_some() {
        prepare_exact_notion_url_path(query)?;
    }

    let results = search_notion_results(query, 1)?;
    let result = results.into_iter().next().ok_or_else(|| {
        if notion_id_from_url(query).is_some() {
            notion_access_miss_message()
        } else {
            "No local Notion page or database matched that search yet. Try a title, path fragment, or Notion URL."
                .to_string()
        }
    })?;
    prioritize_located_notion_result(&result);
    Ok(located_item_for_search_result(result))
}

fn unsupported_notion_locator_url_message(query: &str) -> Option<String> {
    let host = source_url_host(query)?;
    if is_notion_url_host(host.as_str()) {
        return None;
    }

    let source = match url_source_label(&host) {
        Some(label) => label.to_string(),
        None => format!("`{host}`"),
    };
    Some(format!(
        "That looks like a {source} URL. This field opens Notion pages and databases only; paste a Notion page or database URL, title, or mounted Notion path."
    ))
}

fn url_source_label(host: &str) -> Option<&'static str> {
    if host == "github.com" || host.ends_with(".github.com") {
        return Some("GitHub");
    }
    if host == "docs.google.com" || host == "drive.google.com" {
        return Some("Google Docs");
    }
    None
}

fn prepare_exact_notion_url_path(query: &str) -> Result<(), String> {
    let Some(notion_id) = notion_id_from_url(query) else {
        return Ok(());
    };
    let remote_id = RemoteId::new(notion_id.clone());
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Could not load Locality mounts: {error}"))?
        .into_iter()
        .filter(|mount| mount.connector == "notion")
        .collect::<Vec<_>>();
    if mounts.is_empty() {
        return Err("Create a Notion folder before locating pages or databases.".to_string());
    }

    let credentials = open_credential_store(&state_root);
    let mut last_error = None;
    for mount in mounts {
        let source =
            match resolve_source_for_mount_id(&store, credentials.as_ref(), &mount.mount_id) {
                Ok(source) => source,
                Err(error) => {
                    last_error = Some(error.message());
                    continue;
                }
            };
        let ResolvedSource::Notion(connector) = source else {
            continue;
        };
        match connector.resolve_object_path_entries(mount.mount_id.clone(), &remote_id) {
            Ok(entries)
                if entries
                    .iter()
                    .any(|entry| exact_notion_entry_matches(&entry.remote_id, &notion_id)) =>
            {
                save_exact_notion_entries(&mut store, entries)?;
                return Ok(());
            }
            Ok(_) => {
                last_error = Some(format!(
                    "Notion object `{}` was not returned while resolving its parent hierarchy.",
                    remote_id.0
                ));
            }
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }
    }

    Err(last_error.unwrap_or_else(notion_access_miss_message))
}

fn exact_notion_entry_matches(remote_id: &RemoteId, compact_notion_id: &str) -> bool {
    notion_id_from_url(remote_id.as_str()).as_deref() == Some(compact_notion_id)
}

fn save_exact_notion_entries(
    store: &mut SqliteStateStore,
    entries: Vec<TreeEntry>,
) -> Result<(), String> {
    let observed_at = activity_timestamp();
    for entry in entries {
        let existing = store
            .get_entity(&entry.mount_id, &entry.remote_id)
            .map_err(|error| format!("Could not inspect local Notion metadata: {error}"))?;
        let record = exact_located_entity_record(&entry, existing.as_ref())?;
        store
            .save_entity(record)
            .map_err(|error| format!("Could not update local Notion metadata: {error}"))?;

        let mut observation = RemoteObservationRecord::new(
            entry.mount_id.clone(),
            entry.remote_id.clone(),
            entry.kind.clone(),
            entry.title.clone(),
            entry.path.clone(),
            observed_at.clone(),
        );
        if let Some(remote_version) = entry.remote_edited_at.clone() {
            observation = observation.with_remote_version(RemoteVersion::new(remote_version));
        }
        store
            .save_remote_observation(observation)
            .map_err(|error| format!("Could not update local Notion metadata: {error}"))?;
    }

    Ok(())
}

fn exact_located_entity_record(
    entry: &TreeEntry,
    existing: Option<&EntityRecord>,
) -> Result<EntityRecord, String> {
    let mut record = EntityRecord::from(entry.clone());
    if let Some(existing) = existing {
        if existing.path != entry.path
            && matches!(
                existing.hydration,
                HydrationState::Dirty | HydrationState::Conflicted
            )
        {
            return Err(format!(
                "Notion page `{}` moved from `{}` to `{}`, but the old local file has pending changes. Review or push the old file before opening the new path.",
                existing.title,
                display_path(&existing.path),
                display_path(&entry.path)
            ));
        }
        record.hydration = existing.hydration.clone();
        record.content_hash = existing.content_hash.clone();
        if matches!(
            existing.hydration,
            HydrationState::Hydrated | HydrationState::Dirty | HydrationState::Conflicted
        ) {
            record.remote_edited_at = existing.remote_edited_at.clone();
        }
    }
    Ok(record)
}

fn notion_access_miss_message() -> String {
    let Ok(store) = SqliteStateStore::open(default_state_root()) else {
        return "That Notion page or database is outside the selected Notion access for this mount. Use Change Notion Access to select the page, database, or teamspace, then sync the workspace.".to_string();
    };
    let mounts = store.load_mounts().unwrap_or_default();
    let connections = store.list_connections().unwrap_or_default();
    let mount = choose_mount(&mounts, &connections);
    let connection = choose_connection(&connections, mount.as_ref());
    let workspace = connection
        .as_ref()
        .and_then(|connection| connection.workspace_name.clone())
        .or_else(|| {
            mount
                .as_ref()
                .map(|mount| connector_label(&mount.connector))
        })
        .unwrap_or_else(|| "the connected Notion workspace".to_string());
    let scope = mount
        .as_ref()
        .map(|mount| notion_access_scope_label(Some(&store), mount))
        .unwrap_or_else(|| "No mounted Notion access yet".to_string());
    let root_url = mount.as_ref().and_then(notion_access_scope_url);

    notion_access_miss_message_from_parts(&workspace, &scope, root_url.as_deref())
}

fn notion_object_url(id: &str) -> String {
    format!("https://www.notion.so/{}", notion_url_id(id))
}

fn notion_url_id(id: &str) -> String {
    id.chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>()
}

fn notion_access_miss_message_from_parts(
    workspace: &str,
    access_scope: &str,
    root_url: Option<&str>,
) -> String {
    let root_hint = root_url
        .map(|url| format!(" Open the mounted root ({url}) to confirm the current access scope."))
        .unwrap_or_default();
    format!(
        "That Notion page or database is outside the selected Notion access for workspace `{workspace}`. Current mount access: `{access_scope}`.{root_hint} Use Change Notion Access to select this page, database, or the correct teamspace, then sync the workspace."
    )
}

fn notion_access_scope_label(store: Option<&SqliteStateStore>, mount: &MountConfig) -> String {
    let Some(remote_root_id) = mount.remote_root_id.as_ref() else {
        return "Selected pages and databases".to_string();
    };

    let title = store
        .and_then(|store| {
            store
                .get_entity(&mount.mount_id, remote_root_id)
                .ok()
                .flatten()
                .map(|entity| entity.title)
                .or_else(|| {
                    store
                        .get_remote_observation(&mount.mount_id, remote_root_id)
                        .ok()
                        .flatten()
                        .map(|observation| observation.title)
                })
        })
        .filter(|title| !title.trim().is_empty());

    match title {
        Some(title) => title,
        None => format!("Mounted root {}", notion_url_id(&remote_root_id.0)),
    }
}

fn mount_access_scope_label(store: Option<&SqliteStateStore>, mount: &MountConfig) -> String {
    match mount.connector.as_str() {
        "notion" => notion_access_scope_label(store, mount),
        "google-docs" => mount
            .remote_root_id
            .as_ref()
            .map(|remote_id| format!("Drive folder {}", remote_id.0))
            .unwrap_or_else(|| "Google Docs workspace folder".to_string()),
        _ => mount
            .remote_root_id
            .as_ref()
            .map(|remote_id| format!("Remote root {}", remote_id.0))
            .unwrap_or_else(|| "Connector workspace".to_string()),
    }
}

fn notion_access_scope_url(mount: &MountConfig) -> Option<String> {
    mount
        .remote_root_id
        .as_ref()
        .map(|remote_id| notion_object_url(&remote_id.0))
}

fn search_notion_index(query: &str, limit: usize) -> Result<Vec<LocatedItem>, String> {
    Ok(search_notion_results(query, limit)?
        .into_iter()
        .map(located_item_for_search_result)
        .collect::<Vec<_>>())
}

fn search_notion_results(query: &str, limit: usize) -> Result<Vec<SearchResult>, String> {
    let query = query.trim();
    if query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }

    let store = SqliteStateStore::open(default_state_root())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Could not load Locality mounts: {error}"))?
        .into_iter()
        .filter(|mount| mount.connector == "notion")
        .collect::<Vec<_>>();
    if mounts.is_empty() {
        return Err("Create a Notion mount point before locating pages or databases.".to_string());
    }

    Ok(run_search_with_access_roots(
        &store,
        SearchOptions {
            query: query.to_string(),
            connector: Some("notion".to_string()),
            limit,
            include_stale_access: false,
        },
        mount_access_root,
    )
    .map_err(|error| format!("Could not search local Notion index: {}", error.message()))?
    .results)
}

fn prioritize_located_notion_result(result: &SearchResult) {
    if !should_prioritize_located_result(result) {
        return;
    }

    let path = PathBuf::from(&result.absolute_path);
    let request = DaemonRequest::Hydrate {
        mount_id: result.mount_id.clone(),
        remote_id: result.remote_id.clone(),
        path: path.clone(),
    };
    if send_request(&default_state_root(), &request)
        .map(|response| response.ok)
        .unwrap_or(false)
    {
        return;
    }

    let hydration = HydrationRequest::new(
        MountId::new(result.mount_id.clone()),
        RemoteId::new(result.remote_id.clone()),
        path,
        HydrationState::Hydrated,
        HydrationReason::FileOpen,
    );
    if let Ok(mut store) = SqliteStateStore::open(default_state_root()) {
        let _ = store.upsert_hydration_job(HydrationJobRecord::from(hydration));
    }
}

fn should_prioritize_located_result(result: &SearchResult) -> bool {
    result.kind == "page" && result.state == "online_only"
}

fn located_item_for_search_result(result: SearchResult) -> LocatedItem {
    LocatedItem {
        title: result.title,
        kind: search_kind_label(&result.kind).to_string(),
        local_path: display_path(Path::new(&result.absolute_path)),
        state: result.state,
    }
}

fn search_kind_label(kind: &str) -> &str {
    match kind {
        "page" => "Page",
        "database" => "Database",
        "directory" => "Directory",
        "asset" => "Asset",
        "workspace" => "Workspace",
        _ => "Item",
    }
}

fn desktop_settings() -> DesktopSettings {
    let persisted = load_desktop_settings().unwrap_or_default();
    DesktopSettings {
        launch_at_login: cached_launch_at_login_enabled(persisted.launch_at_login),
        show_menu_bar: persisted.show_menu_bar,
    }
}

fn cached_launch_at_login_enabled(default: bool) -> bool {
    LAUNCH_AT_LOGIN_STATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|state| *state)
        .unwrap_or(default)
}

fn set_launch_at_login_cache(enabled: bool) {
    if let Ok(mut state) = LAUNCH_AT_LOGIN_STATE
        .get_or_init(|| Mutex::new(None))
        .lock()
    {
        *state = Some(enabled);
    }
}

fn desktop_launch_requested_background() -> bool {
    desktop_launch_requested_background_from_args(env::args())
}

fn desktop_launch_requested_background_from_args<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .skip(1)
        .any(|arg| arg.as_ref() == DESKTOP_BACKGROUND_LAUNCH_ARG)
}

fn refresh_launch_at_login_cache_async() {
    tauri::async_runtime::spawn_blocking(|| {
        set_launch_at_login_cache(launch_at_login_enabled());
    });
}

fn load_desktop_settings() -> Result<DesktopSettings, String> {
    let path = desktop_settings_path();
    if !path.exists() {
        return Ok(DesktopSettings::default());
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read desktop settings: {error}"))?;
    serde_json::from_str(&contents)
        .map_err(|error| format!("Could not parse desktop settings: {error}"))
}

fn save_desktop_settings(settings: &DesktopSettings) -> Result<(), String> {
    let path = desktop_settings_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create desktop settings folder: {error}"))?;
    }
    let contents = serde_json::to_string_pretty(settings)
        .map_err(|error| format!("Could not serialize desktop settings: {error}"))?;
    fs::write(&path, contents).map_err(|error| format!("Could not write desktop settings: {error}"))
}

fn set_menu_bar_visible(app: &AppHandle, visible: bool) -> Result<ActionReport, String> {
    if let Some(tray) = app.tray_by_id("main") {
        tray.set_visible(visible)
            .map_err(|error| format!("Could not update menu bar icon: {error}"))?;
    }
    if !visible && let Some(window) = app.get_webview_window("tray") {
        let _ = window.hide();
    }

    let mut settings = load_desktop_settings().unwrap_or_default();
    settings.show_menu_bar = visible;
    save_desktop_settings(&settings)?;

    Ok(ActionReport {
        ok: true,
        message: if visible {
            "Locality is shown in the menu bar.".to_string()
        } else {
            "Locality is hidden from the menu bar.".to_string()
        },
    })
}

fn set_launch_at_login(enabled: bool) -> Result<ActionReport, String> {
    if enabled {
        if running_from_read_only_volume()? {
            return Err(
                "Move Locality to Applications before enabling launch at login.".to_string(),
            );
        }
        install_launch_at_login()?;
    } else {
        uninstall_launch_at_login()?;
    }

    let mut settings = load_desktop_settings().unwrap_or_default();
    settings.launch_at_login = enabled;
    save_desktop_settings(&settings)?;
    set_launch_at_login_cache(enabled);

    Ok(ActionReport {
        ok: true,
        message: if enabled {
            "Locality will launch at login.".to_string()
        } else {
            "Locality will not launch at login.".to_string()
        },
    })
}

fn apply_launch_at_login_preference() -> Result<(), String> {
    let settings = load_desktop_settings().unwrap_or_default();
    if settings.launch_at_login && !running_from_read_only_volume()? {
        install_launch_at_login()?;
    }
    set_launch_at_login_cache(settings.launch_at_login);
    Ok(())
}

fn inspect_install_state(state_root: &Path) -> InstallStateReview {
    let marker = load_install_marker(state_root).ok().flatten();
    let sqlite_exists = state_root.join("state.sqlite3").exists();
    let state_exists = state_root.exists();
    let current_build_id = current_desktop_build_id();
    let previous_build_id = marker.as_ref().map(install_marker_display_build_id);
    let should_prompt = marker.is_none() && !sqlite_exists;

    InstallStateReview {
        should_prompt,
        state_exists,
        sqlite_exists,
        previous_build_id,
        current_build_id,
    }
}

fn load_install_marker(state_root: &Path) -> Result<Option<DesktopInstallMarker>, String> {
    let path = install_marker_path(state_root);
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read install marker: {error}"))?;
    serde_json::from_str(&contents)
        .map(Some)
        .map_err(|error| format!("Could not parse install marker: {error}"))
}

fn record_current_install_marker(state_root: &Path) -> Result<(), String> {
    fs::create_dir_all(state_root)
        .map_err(|error| format!("Could not create Locality state folder: {error}"))?;
    let marker = current_install_marker();
    let contents = serde_json::to_string_pretty(&marker)
        .map_err(|error| format!("Could not serialize install marker: {error}"))?;
    fs::write(install_marker_path(state_root), contents)
        .map_err(|error| format!("Could not write install marker: {error}"))
}

fn acknowledge_install_state_at(state_root: &Path) -> Result<ActionReport, String> {
    let previous_marker = load_install_marker(state_root).ok().flatten();
    let should_refresh_guidance =
        install_marker_requires_agent_guidance_refresh(previous_marker.as_ref());
    record_current_install_marker(state_root)?;

    let mut message = "Locality install state recorded.".to_string();
    if should_refresh_guidance {
        message.push(' ');
        message.push_str(&install_or_upgrade_agent_guidance_message(state_root));
    }

    Ok(ActionReport { ok: true, message })
}

fn install_marker_requires_agent_guidance_refresh(marker: Option<&DesktopInstallMarker>) -> bool {
    !matches!(marker, Some(marker) if *marker == current_install_marker())
}

fn install_or_upgrade_agent_guidance_message(state_root: &Path) -> String {
    match refresh_agent_guidance_for_current_mount_at(state_root) {
        Some(report) if report.ok => {
            "Agent guidance was refreshed for the current mount.".to_string()
        }
        Some(report) => {
            let failed = agent_guidance_failed_target_summary(&report);
            if failed.is_empty() {
                "Agent guidance refresh was attempted, but some targets failed.".to_string()
            } else {
                format!("Agent guidance refresh was attempted, but some targets failed: {failed}.")
            }
        }
        None => "Agent guidance will be prepared after a mount is created.".to_string(),
    }
}

fn current_install_marker() -> DesktopInstallMarker {
    DesktopInstallMarker {
        state_format_version: DESKTOP_INSTALL_MARKER_VERSION,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        app_build_id: current_desktop_build_id(),
        daemon_build_id: current_daemon_build_id(),
    }
}

fn install_marker_display_build_id(marker: &DesktopInstallMarker) -> String {
    if marker.app_build_id.is_empty() {
        marker.daemon_build_id.clone()
    } else {
        marker.app_build_id.clone()
    }
}

fn current_desktop_build_id() -> String {
    option_env!("LOCALITY_DESKTOP_BUILD_ID")
        .unwrap_or("unknown")
        .to_string()
}

fn current_daemon_build_id() -> String {
    expected_daemon_build_info().build_id
}

fn install_marker_path(state_root: &Path) -> PathBuf {
    state_root.join("desktop-install.json")
}

fn reset_locality_state_at(state_root: &Path) -> Result<(), String> {
    LOCAL_STATE_RESET_IN_PROGRESS.store(true, Ordering::Release);
    let result = (|| {
        stop_daemon_for_reset(state_root);
        reset_platform_projection_state(state_root)?;
        remove_desktop_support_state()?;
        reset_locality_state_storage(state_root).map_err(|error| error.to_string())?;
        Ok(())
    })();
    LOCAL_STATE_RESET_IN_PROGRESS.store(false, Ordering::Release);
    result
}

fn prepare_locality_uninstall_at(state_root: &Path) -> Result<(), String> {
    reset_locality_state_at(state_root)?;
    let guidance = uninstall_guidance_files();
    if !guidance.ok {
        let details = guidance
            .targets
            .iter()
            .filter(|target| target.status == "failed")
            .map(|target| {
                format!(
                    "{}: {}",
                    target.path.as_deref().unwrap_or(target.agent.as_str()),
                    target.detail
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(if details.is_empty() {
            "Could not remove Locality agent integrations.".to_string()
        } else {
            format!("Could not remove some Locality agent integrations: {details}")
        });
    }
    remove_terminal_cli_links_for_uninstall()?;
    Ok(())
}

fn stop_daemon_for_reset(state_root: &Path) {
    if let Err(error) = run_daemon_control(&daemon_control_args_any_manager("stop", state_root)) {
        desktop_log(
            "warn",
            "reset.stop_localityd_failed",
            format!(
                "could not stop localityd during local state reset: {}",
                error.message()
            ),
        );
    }
}

fn reset_platform_projection_state(state_root: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if let Err(error) = run_macos_file_provider_helper("reset", Vec::new()) {
            desktop_log(
                "warn",
                "reset.file_provider_failed",
                format!(
                    "could not reset macOS File Provider domains during local state reset: {}",
                    error.message()
                ),
            );
        }
    }
    stop_windows_cloud_files_provider_supervisor(state_root)
}

#[cfg(test)]
fn clear_state_root_contents(state_root: &Path) -> Result<(), String> {
    reset_locality_state_storage(state_root)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn remove_desktop_support_state() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let home = home_dir().map_err(|error| format!("HOME is not set: {error}"))?;
        for path in [
            home.join("Library/LaunchAgents/ai.codeflash.locality.localityd.plist"),
            home.join("Library/Group Containers/C484HB7Q6S.group.ai.codeflash.locality"),
            home.join("Library/Group Containers/group.ai.codeflash.locality"),
            home.join("Library/Application Support/ai.codeflash.locality"),
            home.join("Library/Caches/ai.codeflash.locality"),
            home.join("Library/HTTPStorages/ai.codeflash.locality"),
            home.join("Library/Saved Application State/ai.codeflash.locality.savedState"),
        ] {
            remove_path_if_exists(&path)?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    if !path.exists() && !path.is_symlink() {
        return Ok(());
    }
    if path.is_dir() && !path.is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .map_err(|error| format!("Could not remove `{}`: {error}", path.display()))
}

fn start_state_change_watcher(app: AppHandle) {
    tauri::async_runtime::spawn_blocking(move || {
        let state_root = default_state_root();
        if let Err(error) = fs::create_dir_all(&state_root) {
            desktop_log(
                "warn",
                "watcher.create_state_dir_failed",
                format!(
                    "could not create state watch directory `{}`: {error}",
                    state_root.display()
                ),
            );
            return;
        }

        let (tx, rx) = mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                desktop_log(
                    "warn",
                    "watcher.start_failed",
                    format!("could not start state watcher: {error}"),
                );
                return;
            }
        };

        if let Err(error) = watcher.watch(&state_root, RecursiveMode::Recursive) {
            desktop_log(
                "warn",
                "watcher.watch_state_failed",
                format!(
                    "could not watch state directory `{}`: {error}",
                    state_root.display()
                ),
            );
            return;
        }
        let content_roots = watch_virtual_content_roots(&mut watcher, &state_root);

        loop {
            match rx.recv() {
                Ok(Ok(event)) => {
                    let actions = debounce_state_events(&rx, &state_root, &content_roots, event);
                    if actions.wake_live_mode {
                        wake_live_mode_runner();
                    }
                    if actions.refresh_surfaces {
                        refresh_desktop_surfaces(&app);
                    }
                }
                Ok(Err(error)) => desktop_log(
                    "warn",
                    "watcher.event_failed",
                    format!("state watcher event failed: {error}"),
                ),
                Err(_) => break,
            }
        }
    });
}

fn watch_virtual_content_roots(
    watcher: &mut notify::RecommendedWatcher,
    state_root: &Path,
) -> Vec<PathBuf> {
    let content_root = virtual_fs_content_base(state_root);
    if let Err(error) = fs::create_dir_all(&content_root) {
        desktop_log(
            "warn",
            "watcher.create_content_dir_failed",
            format!(
                "could not create virtual content watch directory `{}`: {error}",
                content_root.display()
            ),
        );
        return Vec::new();
    }

    if let Err(error) = watcher.watch(&content_root, RecursiveMode::Recursive) {
        desktop_log(
            "warn",
            "watcher.watch_content_failed",
            format!(
                "could not watch virtual content root `{}`: {error}",
                content_root.display()
            ),
        );
        return Vec::new();
    }

    vec![content_root]
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StateEventActions {
    refresh_surfaces: bool,
    wake_live_mode: bool,
}

impl StateEventActions {
    fn include(&mut self, other: Self) {
        self.refresh_surfaces |= other.refresh_surfaces;
        self.wake_live_mode |= other.wake_live_mode;
    }
}

fn debounce_state_events(
    rx: &mpsc::Receiver<notify::Result<notify::Event>>,
    state_root: &Path,
    content_roots: &[PathBuf],
    first_event: notify::Event,
) -> StateEventActions {
    let mut actions = state_event_actions(&first_event, state_root, content_roots);
    std::thread::sleep(std::time::Duration::from_millis(150));
    while let Ok(event) = rx.try_recv() {
        match event {
            Ok(event) => {
                actions.include(state_event_actions(&event, state_root, content_roots));
            }
            Err(error) => eprintln!("loc desktop state watcher event failed: {error}"),
        }
    }
    actions
}

fn state_event_actions(
    event: &notify::Event,
    state_root: &Path,
    content_roots: &[PathBuf],
) -> StateEventActions {
    let mut actions = StateEventActions::default();
    for path in &event.paths {
        actions.refresh_surfaces |=
            state_event_path_requires_refresh(path, state_root, content_roots);
        actions.wake_live_mode |= state_event_path_wakes_live_mode(path, state_root, content_roots);
    }
    actions
}

fn state_event_path_wakes_live_mode(
    path: &Path,
    state_root: &Path,
    _content_roots: &[PathBuf],
) -> bool {
    is_live_mode_state_change_signal_path(path, state_root)
}

fn state_event_path_requires_refresh(
    path: &Path,
    state_root: &Path,
    content_roots: &[PathBuf],
) -> bool {
    if state_event_path_is_virtual_content_change(path, state_root, content_roots) {
        return true;
    }

    let Ok(relative) = path.strip_prefix(state_root) else {
        return false;
    };
    if relative.as_os_str().is_empty() {
        return false;
    }

    let file_name = relative.file_name().and_then(|name| name.to_str());
    if file_name.is_some_and(|name| name.starts_with("state.sqlite3")) {
        return false;
    }
    if is_live_mode_state_change_signal_path(path, state_root) {
        return true;
    }

    match relative.components().next() {
        Some(std::path::Component::Normal(component)) => match component.to_str() {
            Some("content") => !is_virtual_content_temp_path(relative),
            Some(
                "credentials"
                | "desktop-activity.json"
                | "desktop.json"
                | "desktop-install.json"
                | "logs",
            ) => false,
            _ => false,
        },
        _ => false,
    }
}

fn state_event_path_is_virtual_content_change(
    path: &Path,
    state_root: &Path,
    content_roots: &[PathBuf],
) -> bool {
    if content_roots.iter().any(|root| path.starts_with(root)) {
        return !is_virtual_content_temp_path(path);
    }

    let Ok(relative) = path.strip_prefix(state_root) else {
        return false;
    };
    matches!(
        relative.components().next(),
        Some(std::path::Component::Normal(component)) if component.to_str() == Some("content")
    ) && !is_virtual_content_temp_path(relative)
}

fn is_virtual_content_temp_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.') && name.ends_with(".loc-tmp"))
}

fn refresh_desktop_surfaces(app: &AppHandle) {
    invalidate_desktop_snapshot_cache();
    schedule_desktop_surface_refresh(app.clone());
}

fn schedule_tray_icon_refresh(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let snapshot = tauri::async_runtime::spawn_blocking(load_desktop_snapshot_for_surface)
            .await
            .ok()
            .and_then(Result::ok);
        if let Some(snapshot) = snapshot {
            refresh_tray_icon_for_snapshot(&app, &snapshot);
        } else {
            set_tray_icon_and_tooltip(&app, TrayVisualState::Reconnect, "Locality needs attention");
        }
    });
}

fn schedule_desktop_surface_refresh(app: AppHandle) {
    if LOCAL_STATE_RESET_IN_PROGRESS.load(Ordering::Acquire) {
        return;
    }

    let delay = {
        let mut state = SURFACE_REFRESH_STATE
            .get_or_init(|| Mutex::new(SurfaceRefreshState::default()))
            .lock()
            .expect("surface refresh lock poisoned");
        if state.scheduled {
            return;
        }
        state.scheduled = true;

        let now = Instant::now();
        state
            .last_requested
            .and_then(|last| {
                SURFACE_REFRESH_MIN_INTERVAL.checked_sub(now.saturating_duration_since(last))
            })
            .unwrap_or_default()
    };

    std::thread::spawn(move || {
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }

        {
            let mut state = SURFACE_REFRESH_STATE
                .get_or_init(|| Mutex::new(SurfaceRefreshState::default()))
                .lock()
                .expect("surface refresh lock poisoned");
            state.last_requested = Some(Instant::now());
            state.scheduled = false;
        }

        if !dispatch_window_snapshot_refresh(&app) {
            schedule_tray_icon_refresh(app.clone());
        }
    });
}

fn dispatch_window_snapshot_refresh(app: &AppHandle) -> bool {
    let mut dispatched = false;
    for label in ["main", "tray"] {
        if let Some(window) = app.get_webview_window(label) {
            if !window.is_visible().unwrap_or(false) {
                continue;
            }
            let _ = window.eval("window.dispatchEvent(new CustomEvent('loc-refresh-snapshot'));");
            dispatched = true;
        }
    }
    dispatched
}

fn running_from_read_only_volume() -> Result<bool, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not resolve the Locality app executable: {error}"))?;
    Ok(executable.starts_with("/Volumes"))
}

fn launch_at_login_enabled() -> bool {
    #[cfg(windows)]
    {
        windows_run_key_is_registered().unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        launch_agent_path().is_some_and(|path| path.exists())
    }
}

fn install_launch_at_login() -> Result<(), String> {
    #[cfg(windows)]
    {
        install_windows_login_item()
    }
    #[cfg(not(windows))]
    {
        install_launch_agent()
    }
}

fn uninstall_launch_at_login() -> Result<(), String> {
    #[cfg(windows)]
    {
        uninstall_windows_login_item()
    }
    #[cfg(not(windows))]
    {
        if let Some(path) = launch_agent_path()
            && path.exists()
        {
            fs::remove_file(&path).map_err(|error| {
                format!(
                    "Could not remove launch agent `{}`: {error}",
                    path.display()
                )
            })?;
        }
        Ok(())
    }
}

#[cfg(not(windows))]
fn install_launch_agent() -> Result<(), String> {
    let Some(path) = launch_agent_path() else {
        return Err("HOME is not set, so Locality cannot install a login item.".to_string());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create launch agent folder: {error}"))?;
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not resolve the Locality app executable: {error}"))?;
    let plist = launch_agent_plist(&executable);
    fs::write(&path, plist)
        .map_err(|error| format!("Could not write launch agent `{}`: {error}", path.display()))
}

#[cfg(not(windows))]
fn launch_agent_plist(executable: &Path) -> String {
    let executable = escape_xml(&executable.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>ai.codeflash.locality.desktop</string>
  <key>ProgramArguments</key>
  <array>
    <string>{executable}</string>
    <string>{DESKTOP_BACKGROUND_LAUNCH_ARG}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
"#
    )
}

#[cfg(windows)]
fn install_windows_login_item() -> Result<(), String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not resolve the Locality app executable: {error}"))?;
    let value = windows_run_value_for_executable(&executable);
    let mut command = Command::new("reg");
    configure_hidden_windows_command(&mut command);
    let output = command
        .args([
            "add",
            WINDOWS_RUN_KEY_PATH,
            "/v",
            WINDOWS_RUN_VALUE_NAME,
            "/t",
            "REG_SZ",
            "/d",
        ])
        .arg(value)
        .arg("/f")
        .output()
        .map_err(|error| format!("Could not update the Windows login item: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Could not update the Windows login item: {}",
            process_output_message(&output)
        ))
    }
}

#[cfg(windows)]
fn uninstall_windows_login_item() -> Result<(), String> {
    let mut command = Command::new("reg");
    configure_hidden_windows_command(&mut command);
    let output = command
        .args([
            "delete",
            WINDOWS_RUN_KEY_PATH,
            "/v",
            WINDOWS_RUN_VALUE_NAME,
            "/f",
        ])
        .output()
        .map_err(|error| format!("Could not remove the Windows login item: {error}"))?;
    if output.status.success()
        || process_output_message(&output)
            .to_ascii_lowercase()
            .contains("unable to find")
    {
        Ok(())
    } else {
        Err(format!(
            "Could not remove the Windows login item: {}",
            process_output_message(&output)
        ))
    }
}

#[cfg(windows)]
fn windows_run_key_is_registered() -> Result<bool, String> {
    let mut command = Command::new("reg");
    configure_hidden_windows_command(&mut command);
    let output = command
        .args(["query", WINDOWS_RUN_KEY_PATH, "/v", WINDOWS_RUN_VALUE_NAME])
        .output()
        .map_err(|error| format!("Could not inspect the Windows login item: {error}"))?;
    Ok(output.status.success()
        && String::from_utf8_lossy(&output.stdout).contains(WINDOWS_RUN_VALUE_NAME))
}

#[cfg(windows)]
fn windows_run_value_for_executable(executable: &Path) -> String {
    format!(
        "\"{}\" {DESKTOP_BACKGROUND_LAUNCH_ARG}",
        executable.display()
    )
}

#[cfg(windows)]
fn configure_hidden_windows_command(command: &mut Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(windows)]
fn process_output_message(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }
    format!("process exited with {}", output.status)
}

#[cfg(not(windows))]
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn desktop_settings_path() -> PathBuf {
    default_state_root().join("desktop.json")
}

fn desktop_activity_path(state_root: &Path) -> PathBuf {
    state_root.join("desktop-activity.json")
}

#[cfg(not(windows))]
fn launch_agent_path() -> Option<PathBuf> {
    home_dir().ok().map(|home| {
        home.join("Library")
            .join("LaunchAgents")
            .join("ai.codeflash.locality.desktop.plist")
    })
}

fn action_error(message: String) -> ActionReport {
    ActionReport { ok: false, message }
}

fn desktop_log(level: &str, event: &str, message: impl AsRef<str>) {
    let message = message.as_ref();
    let _ = append_service_log(&default_state_root(), "desktop", level, event, message);
    eprintln!("loc desktop [{event}] {message}");
}

fn mount_access_root(mount: &MountConfig) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if mount.projection == ProjectionMode::MacosFileProvider {
            if macos_path_is_under_cloud_storage(&mount.root) {
                return mount.root.clone();
            }
            if let Ok(url) = macos_file_provider_domain_url(
                localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID,
            ) {
                return url.join(mount_point_directory_name(mount));
            }
        }
    }

    mount.root.clone()
}

#[cfg(target_os = "macos")]
fn mount_root_exists_for_desktop_summary(mount: &MountConfig, path: &Path) -> bool {
    if mount.projection == ProjectionMode::MacosFileProvider
        && macos_path_is_under_cloud_storage(path)
    {
        return true;
    }
    path.exists()
}

#[cfg(not(target_os = "macos"))]
fn mount_root_exists_for_desktop_summary(_mount: &MountConfig, path: &Path) -> bool {
    path.exists()
}

#[cfg(target_os = "macos")]
fn macos_path_is_under_cloud_storage(path: &Path) -> bool {
    path.starts_with(macos_cloud_storage_dir())
}

fn default_state_root() -> PathBuf {
    absolute_state_root(platform_default_state_root())
}

fn expand_tilde(path: &str) -> std::io::Result<PathBuf> {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().map(|home| home.join(rest));
    }
    Ok(PathBuf::from(path))
}

fn home_dir() -> std::io::Result<PathBuf> {
    platform_user_home()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "home is not set"))
}

fn absolute_state_root(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }

    std::env::current_dir()
        .map(|current_dir| current_dir.join(&path))
        .unwrap_or(path)
}

fn display_path(path: &Path) -> String {
    if let Ok(home) = home_dir()
        && let Ok(relative) = path.strip_prefix(&home)
    {
        if relative.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", relative.display());
    }

    path.display().to_string()
}

fn absolute_display_path(path: &Path) -> String {
    absolute_path(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn default_notion_mount_root() -> PathBuf {
    default_notion_shared_root().join(DEFAULT_NOTION_MOUNT_POINT_DIRECTORY)
}

fn default_notion_shared_root() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        macos_locality_cloud_storage_root()
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(home) = home_dir() {
            return home.join("Locality");
        }
        PathBuf::from("Locality")
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        if let Ok(home) = home_dir() {
            return home.join("Locality");
        }
        PathBuf::from("Locality")
    }
}

fn default_notion_access_root() -> PathBuf {
    default_notion_mount_root()
}

#[cfg(target_os = "macos")]
fn macos_cloud_storage_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("Library")
        .join("CloudStorage")
}

#[cfg(target_os = "macos")]
fn macos_locality_cloud_storage_root() -> PathBuf {
    macos_file_provider_cloud_storage_roots()
        .into_iter()
        .next()
        .unwrap_or_else(|| macos_cloud_storage_dir().join("Locality"))
}

#[cfg(target_os = "macos")]
fn macos_file_provider_cloud_storage_roots() -> Vec<PathBuf> {
    let cloud_storage = macos_cloud_storage_dir();
    let mut roots = Vec::new();
    if let Ok(root) =
        macos_file_provider_domain_url(localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID)
    {
        roots.push(root);
    }
    roots.push(cloud_storage.join("Locality"));
    roots.push(cloud_storage.join("Locality"));
    roots.push(cloud_storage.join("Locality-Locality"));
    roots.push(cloud_storage.join("Locality-Locality"));
    dedupe_path_list(roots)
}

#[cfg(target_os = "macos")]
fn dedupe_path_list(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::new();
    for path in paths {
        if !deduped.iter().any(|existing| existing == &path) {
            deduped.push(path);
        }
    }
    deduped
}

fn resolve_mount_root(path: &str) -> Result<PathBuf, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("Choose a folder for the Notion mount.".to_string());
    }
    let root =
        expand_tilde(path).map_err(|error| format!("Could not resolve mount path: {error}"))?;
    absolute_path(&root)
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }

    std::env::current_dir()
        .map(|current_dir| current_dir.join(path))
        .map_err(|error| format!("Could not resolve current directory: {error}"))
}

fn resolve_desktop_mount_root(path: &str) -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let path = path.trim();
        if path.is_empty() {
            return Err("Choose a CloudStorage folder for the source mount.".to_string());
        }
        if !path.contains('/') && !path.starts_with('~') {
            return Ok(macos_locality_cloud_storage_root().join(path));
        }
    }

    let root = resolve_mount_root(path)?;
    normalize_desktop_mount_root(&root)
}

fn normalize_desktop_mount_root(root: &Path) -> Result<PathBuf, String> {
    let root = absolute_path(root)?;

    if root == default_notion_shared_root() {
        return Ok(default_notion_mount_root());
    }

    #[cfg(target_os = "macos")]
    {
        if root == macos_cloud_storage_dir() || root == macos_locality_cloud_storage_root() {
            return Ok(default_notion_mount_root());
        }
        if macos_file_provider_cloud_storage_roots()
            .into_iter()
            .any(|provider_root| root == provider_root)
        {
            return Ok(root.join(DEFAULT_NOTION_MOUNT_POINT_DIRECTORY));
        }
    }

    Ok(root)
}

fn validate_desktop_mount_root(
    root: &Path,
    state_root: &Path,
    projection: &ProjectionMode,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        return validate_desktop_mount_root_with_macos_provider_roots(
            root,
            state_root,
            projection,
            &macos_file_provider_cloud_storage_roots(),
        );
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = projection;
        validate_mount_root(root, state_root)
    }
}

#[cfg(target_os = "macos")]
fn validate_desktop_mount_root_with_macos_provider_roots(
    root: &Path,
    state_root: &Path,
    projection: &ProjectionMode,
    provider_roots: &[PathBuf],
) -> Result<(), String> {
    if *projection == ProjectionMode::MacosFileProvider {
        return validate_macos_file_provider_mount_root(root, state_root, provider_roots);
    }

    let _ = projection;
    validate_mount_root(root, state_root)
}

fn validate_mount_root_location(root: &Path, state_root: &Path) -> Result<PathBuf, String> {
    if root.as_os_str().is_empty() {
        return Err("Choose a folder for the Notion mount.".to_string());
    }

    let root = absolute_path(root)?;
    let state_root = absolute_path(state_root)?;
    if root.starts_with(&state_root) {
        return Err("Choose a folder outside the Locality state directory.".to_string());
    }

    Ok(root)
}

#[cfg(target_os = "macos")]
fn normalize_absolute_mount_path(path: &Path) -> Result<PathBuf, String> {
    let path = absolute_path(path)?;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(Path::new("/")),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(format!(
                    "Could not normalize mount path with parent traversal `{}`.",
                    path.display()
                ));
            }
            std::path::Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

#[cfg(target_os = "macos")]
fn resolved_mount_validation_path(path: &Path) -> Result<PathBuf, String> {
    let normalized = normalize_absolute_mount_path(path)?;
    let mut existing_ancestor = None;
    for candidate in normalized.ancestors() {
        match fs::symlink_metadata(candidate) {
            Ok(_) => {
                existing_ancestor = Some(candidate);
                break;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => {
                return Err(format!(
                    "Could not inspect mount path `{}`: {error}",
                    candidate.display()
                ));
            }
        }
    }
    let existing_ancestor = existing_ancestor
        .ok_or_else(|| format!("No existing parent folder for {}", normalized.display()))?;
    let missing_tail = normalized
        .strip_prefix(existing_ancestor)
        .map_err(|error| {
            format!(
                "Could not resolve mount path `{}`: {error}",
                normalized.display()
            )
        })?;
    let resolved_ancestor = fs::canonicalize(existing_ancestor).map_err(|error| {
        format!(
            "Could not resolve mount path `{}`: {error}",
            existing_ancestor.display()
        )
    })?;
    Ok(resolved_ancestor.join(missing_tail))
}

#[cfg(target_os = "macos")]
fn validate_macos_file_provider_mount_root(
    root: &Path,
    state_root: &Path,
    provider_roots: &[PathBuf],
) -> Result<(), String> {
    if root
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(format!(
            "Choose a mount point inside the Locality File Provider root, for example {}.",
            absolute_display_path(&default_notion_mount_root())
        ));
    }
    let root = validate_mount_root_structure(root, state_root, false)?;
    let root = normalize_absolute_mount_path(&root)?;
    let resolved_root = resolved_mount_validation_path(&root)?;
    let resolved_state_root = resolved_mount_validation_path(state_root)?;
    if resolved_root.starts_with(&resolved_state_root) {
        return Err("Choose a folder outside the Locality state directory.".to_string());
    }
    let provider_roots = provider_roots
        .iter()
        .filter_map(|provider_root| {
            let provider_root = normalize_absolute_mount_path(provider_root).ok()?;
            if fs::symlink_metadata(&provider_root)
                .ok()
                .is_some_and(|metadata| metadata.file_type().is_symlink())
            {
                return None;
            }
            let resolved_provider_root = resolved_mount_validation_path(&provider_root).ok()?;
            Some((provider_root, resolved_provider_root))
        })
        .collect::<Vec<_>>();
    let inside_provider_root =
        provider_roots
            .iter()
            .any(|(provider_root, resolved_provider_root)| {
                root.starts_with(provider_root)
                    && root != *provider_root
                    && resolved_root.starts_with(resolved_provider_root)
                    && resolved_root != *resolved_provider_root
            });
    if !inside_provider_root {
        return Err(format!(
            "Choose a mount point inside the Locality File Provider root, for example {}.",
            absolute_display_path(&default_notion_mount_root())
        ));
    }
    Ok(())
}

fn validate_mount_root_structure(
    root: &Path,
    state_root: &Path,
    require_writable: bool,
) -> Result<PathBuf, String> {
    let root = validate_mount_root_location(root, state_root)?;

    match fs::metadata(&root) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err(format!(
                    "Choose a folder path, not a file: {}",
                    root.display()
                ));
            }
            if require_writable && metadata.permissions().readonly() {
                return Err(format!("Selected folder is read-only: {}", root.display()));
            }
            return Ok(root);
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) => {}
        Err(error) => {
            return Err(format!(
                "Could not inspect mount folder `{}`: {error}",
                root.display()
            ));
        }
    }

    let mut existing_parent = None;
    for candidate in root.ancestors().skip(1) {
        match fs::metadata(candidate) {
            Ok(metadata) => {
                existing_parent = Some((candidate, metadata));
                break;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => {
                return Err(format!(
                    "Could not inspect parent folder `{}`: {error}",
                    candidate.display()
                ));
            }
        }
    }
    let (parent, metadata) = existing_parent
        .ok_or_else(|| format!("No existing parent folder for {}", root.display()))?;
    if !metadata.is_dir() {
        return Err(format!(
            "Mount parent is not a folder: {}",
            parent.display()
        ));
    }
    if require_writable && metadata.permissions().readonly() {
        return Err(format!(
            "Mount parent folder is read-only: {}",
            parent.display()
        ));
    }
    Ok(root)
}

fn validate_mount_root(root: &Path, state_root: &Path) -> Result<(), String> {
    validate_mount_root_structure(root, state_root, true).map(|_| ())
}

fn projection_label(projection: &ProjectionMode) -> &'static str {
    match projection {
        ProjectionMode::PlainFiles => "Plain files",
        ProjectionMode::MacosFileProvider => "macOS File Provider",
        ProjectionMode::LinuxFuse => "Linux FUSE",
        ProjectionMode::WindowsCloudFiles => "Windows Cloud Files",
    }
}

fn connector_label(connector: &str) -> String {
    source_display_name(connector)
}

fn open_in_file_manager(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(path);
        command
    };

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("explorer");
        command.arg(path);
        command
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    };

    command.spawn().map_err(|error| error.to_string())?;
    Ok(())
}

fn schedule_relaunch_after_process_exit(pid: u32, executable: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let target =
            macos_app_bundle_for_exe(executable).unwrap_or_else(|| executable.to_path_buf());
        Command::new("/bin/sh")
            .arg("-c")
            .arg(
                "while kill -0 \"$1\" 2>/dev/null; do sleep 0.2; done; sleep 0.3; exec /usr/bin/open -n \"$2\" --args \"$3\"",
            )
            .arg("locality-update-relaunch")
            .arg(pid.to_string())
            .arg(target)
            .arg(DESKTOP_BACKGROUND_LAUNCH_ARG)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Could not schedule macOS relaunch: {error}"))?;
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        let script = "$ErrorActionPreference = 'SilentlyContinue'; \
            Wait-Process -Id ([int]$args[0]); \
            Start-Sleep -Milliseconds 300; \
            Start-Process -FilePath $args[1] -ArgumentList $args[2]";
        Command::new("powershell.exe")
            .arg("-NoProfile")
            .arg("-WindowStyle")
            .arg("Hidden")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-Command")
            .arg(script)
            .arg(pid.to_string())
            .arg(executable)
            .arg(DESKTOP_BACKGROUND_LAUNCH_ARG)
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Could not schedule Windows relaunch: {error}"))?;
        return Ok(());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = env::var_os("APPIMAGE")
            .map(PathBuf::from)
            .unwrap_or_else(|| executable.to_path_buf());
        Command::new("/bin/sh")
            .arg("-c")
            .arg("while kill -0 \"$1\" 2>/dev/null; do sleep 0.2; done; sleep 0.3; exec \"$2\" \"$3\"")
            .arg("locality-update-relaunch")
            .arg(pid.to_string())
            .arg(target)
            .arg(DESKTOP_BACKGROUND_LAUNCH_ARG)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Could not schedule Linux relaunch: {error}"))?;
        return Ok(());
    }
}

fn macos_app_bundle_for_exe(executable: &Path) -> Option<PathBuf> {
    let macos_dir = executable.parent()?;
    if macos_dir.file_name()? != "MacOS" {
        return None;
    }
    let contents_dir = macos_dir.parent()?;
    if contents_dir.file_name()? != "Contents" {
        return None;
    }
    let app_dir = contents_dir.parent()?;
    if app_dir.extension()? != "app" {
        return None;
    }
    Some(app_dir.to_path_buf())
}

fn open_path_in_vs_code(path: &Path) -> Result<(), String> {
    let command = resolve_vs_code_command().ok_or_else(|| {
        "Could not find VS Code. Install Visual Studio Code and enable the `code` command from the Command Palette: Shell Command: Install 'code' command in PATH.".to_string()
    })?;
    Command::new(&command)
        .arg("--new-window")
        .arg(path)
        .spawn()
        .map_err(|error| format!("Could not open VS Code: {error}"))?;
    Ok(())
}

fn resolve_vs_code_command() -> Option<PathBuf> {
    if let Some(command) = find_command_on_path("code") {
        return Some(command);
    }

    vs_code_command_candidates()
        .into_iter()
        .find(|candidate| candidate.is_file())
}

fn vs_code_command_candidates() -> Vec<PathBuf> {
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Vec::new()
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        let mut candidates = Vec::new();

        #[cfg(target_os = "macos")]
        {
            candidates.push(PathBuf::from(
                "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
            ));
            if let Some(home) = platform_user_home() {
                candidates.push(
                    home.join(
                        "Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
                    ),
                );
            }
            candidates.push(PathBuf::from("/opt/homebrew/bin/code"));
            candidates.push(PathBuf::from("/usr/local/bin/code"));
        }

        #[cfg(target_os = "windows")]
        {
            if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
                candidates.push(
                    PathBuf::from(local_app_data)
                        .join("Programs")
                        .join("Microsoft VS Code")
                        .join("Code.exe"),
                );
            }
        }

        candidates
    }
}

fn find_command_on_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths).find_map(|directory| {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }

        #[cfg(target_os = "windows")]
        {
            let pathext = env::var_os("PATHEXT")
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
            for extension in pathext.split(';') {
                let extension = extension.trim();
                if extension.is_empty() {
                    continue;
                }
                let candidate = directory.join(format!("{name}{extension}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }

        None
    })
}

fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let target = reveal_target(path);
        if !target.exists() {
            return Err(format!("The file {} does not exist.", target.display()));
        }
        reveal_macos_path_in_finder(&target)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let target = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.to_path_buf())
        };
        open_in_file_manager(&target)
    }
}

#[cfg(target_os = "macos")]
fn reveal_target(path: &Path) -> PathBuf {
    if path.exists() || path.extension().is_some() {
        return path.to_path_buf();
    }

    path.with_extension("md")
}

#[cfg(target_os = "macos")]
fn reveal_macos_path_in_finder(path: &Path) -> Result<(), String> {
    Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn reveal_virtual_mount_or_path(path: &Path) -> Result<(), String> {
    if !path.exists()
        && let Some(mount) = virtual_mount_for_path(path)
        && mount.projection.uses_virtual_filesystem()
    {
        return reveal_missing_virtual_mount_path(path, &mount);
    }

    reveal_in_file_manager(path)
}

fn reveal_missing_virtual_mount_path(path: &Path, mount: &MountConfig) -> Result<(), String> {
    match missing_virtual_reveal_action(mount) {
        MissingVirtualRevealAction::RevealRequestedPath => {
            #[cfg(target_os = "macos")]
            {
                let target = reveal_target(path);
                ensure_visible_virtual_file_for_reveal(&target, mount)?;
                signal_virtual_projection_refresh(mount);
                if wait_for_path_to_exist(&target, Duration::from_secs(8)) {
                    return reveal_macos_path_in_finder(&target);
                }
                open_virtual_projection(mount)?;
                Err(format!(
                    "The file is still being materialized. Finder opened the Locality folder; try Reveal again once {} appears.",
                    target.display()
                ))
            }

            #[cfg(not(target_os = "macos"))]
            {
                let _ = path;
                open_virtual_projection(mount)
            }
        }
        MissingVirtualRevealAction::OpenProjectionRoot => open_virtual_projection(mount),
    }
}

#[cfg(target_os = "macos")]
fn ensure_visible_virtual_file_for_reveal(path: &Path, mount: &MountConfig) -> Result<(), String> {
    let Some(path_match) = daemon_file_provider::match_mount_path(mount, path) else {
        return Ok(());
    };

    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let Some(entity) = store
        .find_entity_by_path(&mount.mount_id, &path_match.relative_path)
        .map_err(|error| format!("Could not inspect mounted pages: {error}"))?
    else {
        return Ok(());
    };
    if entity.kind != EntityKind::Page {
        return Ok(());
    }

    let content_root = virtual_fs_content_root(&state_root, &mount.mount_id);
    let content_path = virtual_fs_content_path(&state_root, &mount.mount_id, &entity.path)
        .map_err(|error| format!("Could not resolve the local content cache: {error}"))?;
    if !content_path.exists() {
        if matches!(
            entity.hydration,
            HydrationState::Dirty | HydrationState::Conflicted
        ) {
            return Err(
                "This file has local changes but no materialized cache. Open Pending Changes to resolve it before revealing in Finder."
                    .to_string(),
            );
        }
        let credentials = open_credential_store(&state_root);
        let connector = resolve_source_for_mount_id(&store, credentials.as_ref(), &mount.mount_id)
            .map_err(|error| error.message())?;
        materialize_virtual_fs_item_with_content_root(
            &mut store,
            &connector,
            &content_root,
            &mount.mount_id,
            &entity.remote_id.0,
        )
        .map_err(|error| {
            format!(
                "Could not hydrate `{}` before revealing it: {error}",
                entity.title
            )
        })?;
    }

    write_visible_file_from_cache_for_reveal(&content_path, path)
}

#[cfg(target_os = "macos")]
fn write_visible_file_from_cache_for_reveal(
    cache_path: &Path,
    visible_path: &Path,
) -> Result<(), String> {
    if visible_path.exists() {
        if visible_path.is_file() {
            return Ok(());
        }
        return Err(format!(
            "Could not reveal `{}` because a non-file item already exists there.",
            visible_path.display()
        ));
    }

    let contents = fs::read(cache_path).map_err(|error| {
        format!(
            "Could not read materialized cache `{}`: {error}",
            cache_path.display()
        )
    })?;
    if let Some(parent) = visible_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Could not create Finder folder `{}`: {error}",
                parent.display()
            )
        })?;
    }
    let file_name = visible_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("page.md");
    let temp_path = visible_path.with_file_name(format!(".{file_name}.loc-reveal-tmp"));
    fs::write(&temp_path, contents).map_err(|error| {
        format!(
            "Could not write Finder temp file `{}`: {error}",
            temp_path.display()
        )
    })?;
    fs::rename(&temp_path, visible_path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        format!(
            "Could not materialize Finder file `{}`: {error}",
            visible_path.display()
        )
    })
}

#[cfg(target_os = "macos")]
fn wait_for_path_to_exist(path: &Path, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(125));
    }
    path.exists()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MissingVirtualRevealAction {
    RevealRequestedPath,
    OpenProjectionRoot,
}

fn missing_virtual_reveal_action(mount: &MountConfig) -> MissingVirtualRevealAction {
    if mount.projection == ProjectionMode::MacosFileProvider {
        MissingVirtualRevealAction::RevealRequestedPath
    } else {
        MissingVirtualRevealAction::OpenProjectionRoot
    }
}

fn choose_folder_with_dialog(
    app: &AppHandle,
    current: Option<String>,
) -> Result<Option<PathBuf>, String> {
    let mut dialog = app
        .dialog()
        .file()
        .set_title("Choose where Notion files should appear");
    if let Some(directory) = current
        .as_deref()
        .and_then(|path| expand_tilde(path).ok())
        .filter(|path| path.exists())
    {
        dialog = dialog.set_directory(directory);
    } else {
        #[cfg(target_os = "macos")]
        {
            let loc_root = macos_locality_cloud_storage_root();
            if loc_root.exists() {
                dialog = dialog.set_directory(loc_root);
            } else {
                let cloud_storage = macos_cloud_storage_dir();
                if cloud_storage.exists() {
                    dialog = dialog.set_directory(cloud_storage);
                }
            }
        }
    }

    dialog
        .blocking_pick_folder()
        .map(|path| {
            path.into_path()
                .map_err(|error| format!("Selected folder path was not usable: {error}"))
        })
        .transpose()
}

fn create_desktop_mount_blocking(request: CreateDesktopMountRequest) -> Result<String, String> {
    let state_root = default_state_root();
    let projection = desktop_projection_mode();
    let connector = request.connector.trim().to_string();
    let mount_id = request.mount_id.trim().to_string();
    if mount_id.is_empty() {
        return Err("Mount id is required.".to_string());
    }
    ensure_virtual_projection_domain_available(&projection)?;
    let root = resolve_desktop_mount_root(&request.path)?;
    validate_desktop_mount_root(&root, &state_root, &projection)?;
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let mount_id = MountId::new(mount_id);
    let existing_mount = store
        .get_mount(&mount_id)
        .map_err(|error| format!("Could not inspect existing mounts: {error}"))?
        .map(|mount| mount.connector);
    let can_remount_existing_workspace = existing_mount.as_deref() == Some("notion")
        && connector == "notion"
        && mount_id.0 == "notion-main";
    if existing_mount.is_some() && !can_remount_existing_workspace {
        return Err(format!("Mount id `{}` already exists.", mount_id.0));
    }
    let connection_id = match request
        .connection_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(connection_id) => Some(ConnectionId::new(connection_id.to_string())),
        None => preferred_connection_id_for_connector(&store, &connector)?,
    };
    let remote_root_id = match connector.as_str() {
        "notion" => request
            .notion_root_page
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| RemoteId::new(value.to_string())),
        "google-docs" => {
            let workspace = request
                .google_docs_workspace_folder
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    "Google Docs mounts need a workspace folder name or ID.".to_string()
                })?;
            let temp_mount = MountConfig {
                mount_id: mount_id.clone(),
                connector: connector.clone(),
                root: root.clone(),
                remote_root_id: None,
                connection_id: connection_id.clone(),
                read_only: request.read_only,
                projection: projection.clone(),
                settings_json: "{}".to_string(),
            };
            let credentials = open_credential_store(&state_root);
            let connector =
                resolve_google_docs_connector_for_mount(&store, credentials.as_ref(), &temp_mount)
                    .map_err(|error| error.message())?;
            let folder_id = connector
                .resolve_workspace_folder(workspace)
                .map_err(|error| {
                    format!("Failed to resolve Google Docs workspace folder `{workspace}`: {error}")
                })?;
            Some(folder_id)
        }
        "gmail" | "granola" => None,
        other => {
            return Err(format!(
                "Desktop mount creation does not support connector `{other}`."
            ));
        }
    };
    let preserved = if can_remount_existing_workspace {
        prepare_existing_workspace_mount_for_remount(&mut store, &state_root, &mount_id)?
    } else {
        None
    };

    let mount_report = run_mount(
        &mut store,
        MountOptions {
            mount_id,
            connector,
            root,
            remote_root_id,
            connection_id,
            read_only: request.read_only,
            projection: projection.clone(),
            settings_json: "{}".to_string(),
        },
    )
    .map_err(|error| error.message())?;

    ensure_daemon_running(&state_root)?;
    reload_daemon_mounts(&state_root)?;

    let mount = store
        .get_mount(&MountId::new(mount_report.mount_id.clone()))
        .map_err(|error| format!("Could not reload created mount: {error}"))?
        .ok_or_else(|| "Created mount was not found in Locality state.".to_string())?;

    if mount.projection.uses_virtual_filesystem() {
        activate_virtual_projection_mount(&state_root, &mount, true)?;
    }

    let mut message = format!(
        "Mounted {} at {} with {}.",
        connector_label(&mount.connector),
        absolute_display_path(&mount_access_root(&mount)),
        projection_label(&mount.projection)
    );
    if let Some(preserved) = preserved {
        message.push_str(&format!(
            " Locality preserved {} pending local change{} at `{}` and cleared the old cached paths before remounting.",
            preserved.count,
            if preserved.count == 1 { "" } else { "s" },
            preserved.directory.display()
        ));
    }

    Ok(message)
}

fn desktop_mount_by_id(store: &SqliteStateStore, mount_id: &str) -> Result<MountConfig, String> {
    store
        .get_mount(&MountId::new(mount_id.to_string()))
        .map_err(|error| format!("Could not load mount `{mount_id}`: {error}"))?
        .ok_or_else(|| format!("No Locality mount has id `{mount_id}`."))
}

fn preferred_connection_id_for_connector(
    store: &SqliteStateStore,
    connector: &str,
) -> Result<Option<ConnectionId>, String> {
    if connector == "notion" {
        return preferred_notion_connection_id(store);
    }

    if let Some(connection_id) = store
        .load_mounts()
        .map_err(|error| format!("Could not load mounts: {error}"))?
        .into_iter()
        .find(|mount| mount.connector == connector)
        .and_then(|mount| mount.connection_id)
    {
        return Ok(Some(connection_id));
    }

    let connections = store
        .list_connections()
        .map_err(|error| format!("Could not load connections: {error}"))?;
    Ok(connections
        .iter()
        .find(|connection| connection.connector == connector && connection.status == "active")
        .or_else(|| {
            connections
                .iter()
                .find(|connection| connection.connector == connector)
        })
        .map(|connection| connection.connection_id.clone()))
}

fn preferred_notion_connection_id(
    store: &SqliteStateStore,
) -> Result<Option<ConnectionId>, String> {
    let existing_mount_connection = store
        .load_mounts()
        .map_err(|error| format!("Could not load mounts: {error}"))?
        .into_iter()
        .find(|mount| mount.mount_id.0 == "notion-main" && mount.connector == "notion")
        .and_then(|mount| mount.connection_id);
    let active = store
        .list_connections()
        .map_err(|error| format!("Could not load connections: {error}"))?
        .into_iter()
        .filter(|connection| connection.connector == "notion" && connection.status == "active")
        .collect::<Vec<_>>();

    if let Some(existing) = existing_mount_connection
        && active
            .iter()
            .any(|connection| connection.connection_id == existing)
    {
        return Ok(Some(existing));
    }

    if let Some(connection) = active.first() {
        return Ok(Some(connection.connection_id.clone()));
    }
    if std::env::var("NOTION_TOKEN").is_ok() {
        return Ok(None);
    }
    Err("Connect Notion before creating the workspace mount.".to_string())
}

fn desktop_projection_mode() -> ProjectionMode {
    #[cfg(target_os = "macos")]
    {
        ProjectionMode::MacosFileProvider
    }
    #[cfg(target_os = "linux")]
    {
        ProjectionMode::LinuxFuse
    }
    #[cfg(target_os = "windows")]
    {
        ProjectionMode::WindowsCloudFiles
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        ProjectionMode::PlainFiles
    }
}

fn ensure_daemon_running(state_root: &Path) -> Result<(), String> {
    let _guard = daemon_lifecycle_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ensure_daemon_running_locked(state_root)
}

fn daemon_lifecycle_lock() -> &'static Mutex<()> {
    DAEMON_LIFECYCLE_LOCK.get_or_init(|| Mutex::new(()))
}

fn ensure_daemon_running_locked(state_root: &Path) -> Result<(), String> {
    let current_build = expected_daemon_build_info();
    match running_daemon_build(state_root) {
        Some(build)
            if build == current_build && running_daemon_predates_bundled_binary(state_root) =>
        {
            desktop_log(
                "warn",
                "daemon.stale_same_build",
                format!(
                    "detected running localityd build {} from before the bundled binary was installed; restarting localityd",
                    build.build_id
                ),
            );
            return restart_daemon_for_current_binary(state_root, &current_build);
        }
        Some(build) if build == current_build => return Ok(()),
        Some(build) => {
            desktop_log(
                "warn",
                "daemon.build_mismatch",
                format!(
                    "detected running localityd build {} but bundled localityd is {}; restarting localityd",
                    build.build_id, current_build.build_id
                ),
            );
            return restart_daemon_for_current_binary(state_root, &current_build);
        }
        None if daemon_is_ready(state_root) => {
            desktop_log(
                "warn",
                "daemon.missing_build_metadata",
                "detected an older localityd without build metadata; restarting localityd",
            );
            return restart_daemon_for_current_binary(state_root, &current_build);
        }
        None => {}
    }

    clear_stale_daemon_manager(state_root);
    start_daemon_for_current_binary(state_root)
}

fn running_daemon_predates_bundled_binary(state_root: &Path) -> bool {
    let Some(binary) = bundled_localityd_binary() else {
        return false;
    };
    let pid_file = state_root.join(DAEMON_PID_FILENAME);
    let pid_modified = file_modified_time(&pid_file);
    let binary_modified = file_modified_time(&binary);
    daemon_process_started_before_bundled_binary(pid_modified, binary_modified)
}

fn daemon_process_started_before_bundled_binary(
    pid_modified: Option<SystemTime>,
    binary_modified: Option<SystemTime>,
) -> bool {
    matches!((pid_modified, binary_modified), (Some(pid), Some(binary)) if pid < binary)
}

fn file_modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn start_daemon_for_current_binary(state_root: &Path) -> Result<(), String> {
    let report = run_daemon_control(&daemon_control_args("start", state_root))
        .map_err(|error| format!("Could not start localityd: {}", error.message()))?;
    if report.state == DaemonRunState::Running {
        Ok(())
    } else {
        Err("localityd did not start.".to_string())
    }
}

fn clear_stale_daemon_manager(state_root: &Path) {
    match run_daemon_control(&daemon_control_args_any_manager("stop", state_root)) {
        Ok(report) if report.state == DaemonRunState::Stopped => {
            if report.message != "daemon was not running" {
                desktop_log(
                    "info",
                    "daemon.stale_manager_removed",
                    format!(
                        "cleared existing {} daemon manager before starting bundled localityd",
                        report.manager.as_str()
                    ),
                );
            }
        }
        Ok(_) => {}
        Err(error) => desktop_log(
            "warn",
            "daemon.stale_manager_remove_failed",
            format!(
                "could not clear stale daemon manager before starting bundled localityd: {}",
                error.message()
            ),
        ),
    }
}

fn restart_daemon_for_current_binary(
    state_root: &Path,
    expected_build: &DaemonBuildInfo,
) -> Result<(), String> {
    let stop_error = run_daemon_control(&daemon_control_args_any_manager("stop", state_root))
        .err()
        .map(|error| error.message());
    if let Some(message) = stop_error.as_ref() {
        desktop_log(
            "warn",
            "daemon.stop_before_restart_failed",
            format!("could not stop existing localityd before restart: {message}"),
        );
    }
    start_daemon_for_current_binary(state_root)?;
    match running_daemon_build(state_root) {
        Some(build) if &build == expected_build => Ok(()),
        Some(build) => {
            let stop_detail = stop_error
                .map(|message| format!(" Previous stop attempt failed: {message}"))
                .unwrap_or_default();
            Err(format!(
                "localityd restarted, but reported build {} instead of {}.{}",
                build.build_id, expected_build.build_id, stop_detail
            ))
        }
        None => Err("localityd restarted, but did not report build metadata.".to_string()),
    }
}

fn expected_daemon_build_info() -> DaemonBuildInfo {
    let Some(binary) = bundled_localityd_binary() else {
        return DaemonBuildInfo::current();
    };
    match daemon_build_info_from_binary(&binary) {
        Ok(build) => build,
        Err(message) => {
            desktop_log(
                "warn",
                "daemon.bundled_build_probe_failed",
                format!(
                    "could not read bundled localityd build metadata from {}: {}; falling back to linked daemon build metadata",
                    binary.display(),
                    message
                ),
            );
            DaemonBuildInfo::current()
        }
    }
}

fn daemon_build_info_from_binary(binary: &Path) -> Result<DaemonBuildInfo, String> {
    let mut command = Command::new(binary);
    command.arg("--build-info");
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("process exited with {}", output.status)
        } else {
            stderr
        });
    }
    parse_daemon_build_info_json(&output.stdout)
}

fn parse_daemon_build_info_json(output: &[u8]) -> Result<DaemonBuildInfo, String> {
    serde_json::from_slice(output).map_err(|error| error.to_string())
}

fn daemon_control_args(action: &str, state_root: &Path) -> Vec<String> {
    let mut args = vec![
        action.to_string(),
        "--session".to_string(),
        "--state-dir".to_string(),
        state_root.display().to_string(),
    ];
    if let Ok(tcp_addr) = std::env::var("LOCALITY_DAEMON_TCP_ADDR")
        && !tcp_addr.is_empty()
    {
        args.push("--tcp-addr".to_string());
        args.push(tcp_addr);
    }
    if let Some(localityd_bin) = bundled_localityd_binary() {
        args.push("--localityd-bin".to_string());
        args.push(localityd_bin.display().to_string());
    }
    args
}

fn daemon_control_args_any_manager(action: &str, state_root: &Path) -> Vec<String> {
    let mut args = vec![
        action.to_string(),
        "--state-dir".to_string(),
        state_root.display().to_string(),
    ];
    if let Ok(tcp_addr) = std::env::var("LOCALITY_DAEMON_TCP_ADDR")
        && !tcp_addr.is_empty()
    {
        args.push("--tcp-addr".to_string());
        args.push(tcp_addr);
    }
    args
}

fn bundled_localityd_binary() -> Option<PathBuf> {
    bundled_binary_next_to_current_exe("localityd")
}

fn bundled_loc_cli_binary() -> Option<PathBuf> {
    bundled_binary_next_to_current_exe("loc")
}

fn app_store_distribution() -> bool {
    option_env!("LOCALITY_DISTRIBUTION_CHANNEL")
        .is_some_and(|channel| channel.eq_ignore_ascii_case("mas"))
}

fn desktop_smoke_test_requested() -> bool {
    std::env::var_os("LOCALITY_DESKTOP_SMOKE_TEST").is_some()
}

fn install_terminal_cli_link() -> Result<PathBuf, String> {
    if app_store_distribution() {
        if let Some(path) = find_command_in_path("loc") {
            return Ok(path);
        }
        return Err(
            "The Mac App Store build does not install a terminal command. Install Locality from Homebrew or the direct download to use the bundled CLI."
                .to_string(),
        );
    }

    if running_from_read_only_volume()? {
        if let Some(path) = find_command_in_path("loc") {
            return Ok(path);
        }
        return Err(
            "Move Locality to Applications before installing the terminal command.".to_string(),
        );
    }

    let Some(cli_path) = bundled_loc_cli_binary() else {
        if let Some(path) = find_command_in_path("loc") {
            return Ok(path);
        }
        return Err("The packaged Locality CLI was not found in this app bundle.".to_string());
    };

    install_terminal_cli_link_in_path(&cli_path)
}

#[derive(Debug, PartialEq, Eq)]
enum TerminalCliLinkState {
    Current,
    NeedsInstall,
}

fn terminal_cli_command_filename() -> &'static str {
    #[cfg(windows)]
    {
        "loc.cmd"
    }
    #[cfg(not(windows))]
    {
        "loc"
    }
}

fn install_terminal_cli_link_at(cli_path: &Path, link_path: &Path) -> Result<PathBuf, String> {
    let cli_path = absolute_path(cli_path)?;
    if !cli_path.is_file() {
        return Err(format!(
            "The bundled Locality CLI was not found at {}.",
            cli_path.display()
        ));
    }

    match terminal_cli_link_state(link_path, &cli_path)? {
        TerminalCliLinkState::Current => return Ok(link_path.to_path_buf()),
        TerminalCliLinkState::NeedsInstall => {}
    }

    if let Err(error) = install_terminal_cli_link_direct(&cli_path, link_path) {
        return Err(format!(
            "Could not install the Locality terminal command at {}: {error}",
            link_path.display()
        ));
    }

    match terminal_cli_link_state(link_path, &cli_path)? {
        TerminalCliLinkState::Current => Ok(link_path.to_path_buf()),
        TerminalCliLinkState::NeedsInstall => Err(format!(
            "Could not verify the Locality terminal command at {}.",
            link_path.display()
        )),
    }
}

fn install_terminal_cli_link_in_path(cli_path: &Path) -> Result<PathBuf, String> {
    let mut dirs = terminal_cli_path_dirs();
    sort_terminal_cli_path_dirs(&mut dirs);
    let user_fallback = default_user_terminal_cli_dir();
    if let Some(directory) = user_fallback.as_deref() {
        insert_user_terminal_cli_fallback_dir(&mut dirs, directory);
    }
    let installed = install_terminal_cli_link_in_sorted_path_dirs(cli_path, dirs)?;
    if user_fallback.as_deref().is_some_and(|directory| {
        installed
            .parent()
            .is_some_and(|parent| paths_equal(parent, directory))
    }) {
        ensure_terminal_cli_dir_registered(&installed)?;
    }
    Ok(installed)
}

#[cfg(test)]
fn install_terminal_cli_link_in_path_dirs(
    cli_path: &Path,
    mut dirs: Vec<PathBuf>,
) -> Result<PathBuf, String> {
    sort_terminal_cli_path_dirs(&mut dirs);
    install_terminal_cli_link_in_sorted_path_dirs(cli_path, dirs)
}

fn install_terminal_cli_link_in_sorted_path_dirs(
    cli_path: &Path,
    dirs: Vec<PathBuf>,
) -> Result<PathBuf, String> {
    let mut errors = Vec::new();
    let mut checked = Vec::new();

    for directory in dirs {
        checked.push(directory.display().to_string());
        if is_protected_terminal_cli_path(&directory) {
            continue;
        }
        let link_path = directory.join(terminal_cli_command_filename());
        match install_terminal_cli_link_at(cli_path, &link_path) {
            Ok(path) => return Ok(path),
            Err(error) => errors.push(error),
        }
    }

    let checked = if checked.is_empty() {
        "none".to_string()
    } else {
        checked.join(", ")
    };
    let detail = errors
        .last()
        .map(|error| format!(" Last error: {error}"))
        .unwrap_or_default();

    Err(format!(
        "Could not install the Locality terminal command without administrator privileges. Add a user-writable directory such as ~/.local/bin to PATH, then try again. Checked PATH directories: {checked}.{detail}"
    ))
}

fn remove_terminal_cli_links_for_uninstall() -> Result<(), String> {
    let Some(cli_path) = bundled_loc_cli_binary() else {
        return Ok(());
    };
    let mut dirs = terminal_cli_path_dirs();
    if let Some(directory) = default_user_terminal_cli_dir() {
        dirs.push(directory);
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
    }
    dirs.sort();
    dirs.dedup();

    let mut errors = Vec::new();
    for directory in dirs {
        let link_path = directory.join(terminal_cli_command_filename());
        if let Err(error) = remove_terminal_cli_link_at(&cli_path, &link_path) {
            errors.push(error);
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn remove_terminal_cli_link_at(cli_path: &Path, link_path: &Path) -> Result<bool, String> {
    match terminal_cli_link_state(link_path, cli_path) {
        Ok(TerminalCliLinkState::Current) => {}
        Ok(TerminalCliLinkState::NeedsInstall) => return Ok(false),
        Err(_error) => return Ok(false),
    }

    fs::remove_file(link_path).map_err(|error| {
        format!(
            "Could not remove Locality terminal command {}: {error}",
            link_path.display()
        )
    })?;
    Ok(true)
}

#[cfg(not(windows))]
fn default_user_terminal_cli_dir() -> Option<PathBuf> {
    home_dir().ok().map(|home| home.join(".local/bin"))
}

#[cfg(windows)]
fn default_user_terminal_cli_dir() -> Option<PathBuf> {
    env_first(&["LOCALAPPDATA"])
        .map(PathBuf::from)
        .map(|local_app_data| {
            let windows_apps = local_app_data.join("Microsoft").join("WindowsApps");
            if terminal_cli_dir_is_on_path(&windows_apps) {
                windows_apps
            } else {
                local_app_data.join("Locality").join("bin")
            }
        })
        .or_else(|| {
            home_dir().ok().map(|home| {
                home.join("AppData")
                    .join("Local")
                    .join("Locality")
                    .join("bin")
            })
        })
}

fn insert_user_terminal_cli_fallback_dir(dirs: &mut Vec<PathBuf>, directory: &Path) {
    if dirs.iter().any(|existing| paths_equal(existing, directory)) {
        return;
    }
    let index = dirs
        .iter()
        .position(|candidate| is_protected_terminal_cli_path(candidate))
        .unwrap_or(dirs.len());
    dirs.insert(index, directory.to_path_buf());
}

fn ensure_terminal_cli_dir_registered(installed: &Path) -> Result<(), String> {
    let directory = installed.parent().ok_or_else(|| {
        format!(
            "Could not determine parent directory for {}.",
            installed.display()
        )
    })?;
    if terminal_cli_dir_is_on_path(directory) {
        return Ok(());
    }

    #[cfg(windows)]
    {
        return Err(format!(
            "Installed the Locality terminal command at {}, but {} is not on PATH. Add that directory to your user PATH, then open a new terminal.",
            installed.display(),
            directory.display()
        ));
    }

    #[cfg(not(windows))]
    {
        let Some(config_path) = terminal_cli_shell_config_path() else {
            return Err(format!(
                "Installed the Locality terminal command at {}, but could not find your home directory to add it to PATH.",
                installed.display()
            ));
        };
        write_terminal_cli_path_section(&config_path, directory).map_err(|error| {
        format!(
            "Installed the Locality terminal command at {}, but could not update {} to add it to PATH: {error}",
            installed.display(),
            config_path.display()
        )
    })
    }
}

fn terminal_cli_dir_is_on_path(directory: &Path) -> bool {
    std::env::var_os("PATH").is_some_and(|path| path_list_contains_dir(path, directory))
        || login_shell_path().is_some_and(|path| path_list_contains_dir(path, directory))
}

fn path_list_contains_dir(path: std::ffi::OsString, directory: &Path) -> bool {
    std::env::split_paths(&path).any(|candidate| paths_equal(&candidate, directory))
}

#[cfg(not(windows))]
fn terminal_cli_shell_config_path() -> Option<PathBuf> {
    let home = home_dir().ok()?;
    let shell_name = std::env::var_os("SHELL")
        .and_then(|shell| PathBuf::from(shell).file_name().map(|name| name.to_owned()))
        .and_then(|name| name.to_str().map(|name| name.to_string()));
    Some(match shell_name.as_deref() {
        Some("bash") => home.join(".bash_profile"),
        Some("zsh") | None => home.join(".zprofile"),
        _ => home.join(".profile"),
    })
}

#[cfg(any(not(windows), test))]
fn write_terminal_cli_path_section(path: &Path, directory: &Path) -> Result<(), String> {
    let existing = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.to_string()),
    };
    let block = terminal_cli_path_shell_block(directory);
    let next = replace_terminal_cli_path_section(&existing, &block);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(path, next).map_err(|error| error.to_string())
}

#[cfg(any(not(windows), test))]
fn terminal_cli_path_shell_block(directory: &Path) -> String {
    let directory = shell_single_quote(&directory.display().to_string());
    format!(
        "{TERMINAL_CLI_PATH_MANAGED_START}\n\
_loc_cli_dir={directory}\n\
case \":$PATH:\" in\n\
  *\":$_loc_cli_dir:\"*) ;;\n\
  *) export PATH=\"$_loc_cli_dir:$PATH\" ;;\n\
esac\n\
unset _loc_cli_dir\n\
{TERMINAL_CLI_PATH_MANAGED_END}\n"
    )
}

#[cfg(any(not(windows), test))]
fn replace_terminal_cli_path_section(existing: &str, block: &str) -> String {
    let Some(start) = existing.find(TERMINAL_CLI_PATH_MANAGED_START) else {
        let trimmed = existing.trim_end();
        return if trimmed.is_empty() {
            block.to_string()
        } else {
            format!("{trimmed}\n\n{block}")
        };
    };
    let Some(relative_end) = existing[start..].find(TERMINAL_CLI_PATH_MANAGED_END) else {
        let trimmed = existing.trim_end();
        return format!("{trimmed}\n\n{block}");
    };
    let end = start + relative_end + TERMINAL_CLI_PATH_MANAGED_END.len();
    let mut next = String::new();
    next.push_str(existing[..start].trim_end());
    if !next.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(block);
    let suffix = existing[end..].trim_start_matches(['\r', '\n']);
    if !suffix.is_empty() {
        next.push('\n');
        next.push_str(suffix);
    }
    next
}

#[cfg(any(not(windows), test))]
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn terminal_cli_path_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        append_path_dirs(&mut dirs, path);
    }
    if let Some(path) = login_shell_path() {
        append_path_dirs(&mut dirs, path);
    }
    dirs
}

fn sort_terminal_cli_path_dirs(dirs: &mut Vec<PathBuf>) {
    let home = home_dir().ok();
    let mut indexed = std::mem::take(dirs)
        .into_iter()
        .enumerate()
        .collect::<Vec<_>>();
    indexed.sort_by_key(|(index, directory)| {
        (
            terminal_cli_path_dir_rank(directory, home.as_deref()),
            *index,
        )
    });
    dirs.extend(indexed.into_iter().map(|(_, directory)| directory));
}

fn terminal_cli_path_dir_rank(directory: &Path, home: Option<&Path>) -> u8 {
    #[cfg(windows)]
    {
        if is_windows_user_terminal_cli_path(directory) {
            return 0;
        }
    }

    if directory.ends_with(Path::new(".local/bin"))
        || home.is_some_and(|home| {
            directory == home.join("bin") || directory == home.join(".local/bin")
        })
    {
        return 0;
    }
    if home.is_some_and(|home| directory.starts_with(home)) {
        return 1;
    }
    if is_homebrew_path(directory) {
        return 3;
    }
    if is_system_path(directory) {
        return 4;
    }
    2
}

fn is_homebrew_path(path: &Path) -> bool {
    let value = path.display().to_string().to_ascii_lowercase();
    value == "/opt/homebrew/bin" || value == "/usr/local/bin" || value.contains("/homebrew/")
}

fn is_system_path(path: &Path) -> bool {
    matches!(
        path.to_str(),
        Some("/usr/bin" | "/bin" | "/usr/sbin" | "/sbin")
    )
}

fn is_protected_terminal_cli_path(path: &Path) -> bool {
    #[cfg(windows)]
    {
        if is_windows_system_path(path) {
            return true;
        }
    }

    is_system_path(path)
        || path.starts_with("/System")
        || path.starts_with("/var/run/com.apple.security.cryptexd")
}

#[cfg(windows)]
fn is_windows_user_terminal_cli_path(path: &Path) -> bool {
    let value = path
        .display()
        .to_string()
        .replace('/', "\\")
        .to_ascii_lowercase();
    value.ends_with(r"\microsoft\windowsapps") || value.ends_with(r"\locality\bin")
}

#[cfg(windows)]
fn is_windows_system_path(path: &Path) -> bool {
    let value = path
        .display()
        .to_string()
        .replace('/', "\\")
        .to_ascii_lowercase();
    value.starts_with(r"c:\windows")
        || value.starts_with(r"c:\program files")
        || value.starts_with(r"c:\program files (x86)")
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    left == right
        || match (left.canonicalize(), right.canonicalize()) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

fn append_path_dirs(dirs: &mut Vec<PathBuf>, path: std::ffi::OsString) {
    for directory in std::env::split_paths(&path) {
        if !directory.is_absolute() || dirs.iter().any(|existing| existing == &directory) {
            continue;
        }
        dirs.push(directory);
    }
}

fn terminal_cli_link_state(
    link_path: &Path,
    cli_path: &Path,
) -> Result<TerminalCliLinkState, String> {
    let metadata = match fs::symlink_metadata(link_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(TerminalCliLinkState::NeedsInstall);
        }
        Err(error) => {
            return Err(format!(
                "Could not inspect terminal command {}: {error}",
                link_path.display()
            ));
        }
    };

    #[cfg(windows)]
    if let Some(state) = windows_terminal_cli_shim_state(link_path, &cli_path, &metadata)? {
        return Ok(state);
    }

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(link_path).map_err(|error| {
            format!(
                "Could not read terminal command link {}: {error}",
                link_path.display()
            )
        })?;
        let target = if target.is_absolute() {
            target
        } else {
            link_path
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .join(target)
        };
        return Ok(if paths_refer_to_same_file(&target, cli_path) {
            TerminalCliLinkState::Current
        } else {
            TerminalCliLinkState::NeedsInstall
        });
    }

    if paths_refer_to_same_file(link_path, cli_path) {
        return Ok(TerminalCliLinkState::Current);
    }

    Err(format!(
        "A file already exists at {}. Move it aside so Locality can install the bundled CLI there.",
        link_path.display()
    ))
}

fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    let Ok(left) = left.canonicalize() else {
        return false;
    };
    let Ok(right) = right.canonicalize() else {
        return false;
    };
    left == right
}

#[cfg(windows)]
fn windows_terminal_cli_shim_state(
    link_path: &Path,
    cli_path: &Path,
    metadata: &fs::Metadata,
) -> Result<Option<TerminalCliLinkState>, String> {
    if !metadata.is_file() || !path_extension_eq(link_path, "cmd") {
        return Ok(None);
    }

    let contents = fs::read_to_string(link_path).map_err(|error| {
        format!(
            "Could not read terminal command shim {}: {error}",
            link_path.display()
        )
    })?;
    if !contents.contains(WINDOWS_TERMINAL_CLI_SHIM_MARKER) {
        return Err(format!(
            "A file already exists at {}. Move it aside so Locality can install the bundled CLI there.",
            link_path.display()
        ));
    }

    Ok(Some(
        if contents == windows_terminal_cli_shim_contents(cli_path) {
            TerminalCliLinkState::Current
        } else {
            TerminalCliLinkState::NeedsInstall
        },
    ))
}

#[cfg(windows)]
fn windows_terminal_cli_shim_contents(cli_path: &Path) -> String {
    let cli_path = batch_file_literal(&cli_path.display().to_string());
    format!(
        "@echo off\r\nrem {WINDOWS_TERMINAL_CLI_SHIM_MARKER}\r\nset \"_loc_cli={cli_path}\"\r\n\"%_loc_cli%\" %*\r\n"
    )
}

#[cfg(windows)]
fn batch_file_literal(value: &str) -> String {
    value.replace('%', "%%")
}

#[cfg(windows)]
fn path_extension_eq(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

#[cfg(unix)]
fn install_terminal_cli_link_direct(cli_path: &Path, link_path: &Path) -> io::Result<()> {
    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(link_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_file(link_path)?,
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::os::unix::fs::symlink(cli_path, link_path)
}

#[cfg(windows)]
fn install_terminal_cli_link_direct(cli_path: &Path, link_path: &Path) -> io::Result<()> {
    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(link_path) {
        Ok(_) => fs::remove_file(link_path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::write(link_path, windows_terminal_cli_shim_contents(cli_path))
}

#[cfg(not(any(unix, windows)))]
fn install_terminal_cli_link_direct(_cli_path: &Path, _link_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "terminal command links are only supported on Unix and Windows",
    ))
}

#[cfg(target_os = "macos")]
fn login_shell_path() -> Option<std::ffi::OsString> {
    let shell = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"));
    let output = Command::new(shell)
        .args(["-lc", "printf '%s' \"$PATH\""])
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    Some(std::ffi::OsString::from(
        String::from_utf8_lossy(&output.stdout).to_string(),
    ))
}

#[cfg(not(target_os = "macos"))]
fn login_shell_path() -> Option<std::ffi::OsString> {
    None
}

fn find_command_in_path(command: &str) -> Option<PathBuf> {
    if command.contains('/') || command.contains('\\') {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    let candidates = command_path_candidates(command);
    for directory in std::env::split_paths(&path) {
        for candidate in &candidates {
            let path = directory.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn command_path_candidates(command: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        if Path::new(command).extension().is_some() {
            return vec![command.to_string()];
        }
        return vec![
            format!("{command}.exe"),
            format!("{command}.cmd"),
            format!("{command}.bat"),
            command.to_string(),
        ];
    }

    #[cfg(not(windows))]
    {
        vec![command.to_string()]
    }
}

fn reload_daemon_mounts(state_root: &Path) -> Result<(), String> {
    match reload_daemon_mounts_once(state_root) {
        Ok(()) => Ok(()),
        Err(error) if error.is_unsupported_schema_version() => {
            desktop_log(
                "warn",
                "daemon.reload_stale_schema",
                format!(
                    "detected a stale localityd schema reader during reload: {}",
                    error.message
                ),
            );
            let current_build = expected_daemon_build_info();
            restart_daemon_for_current_binary(state_root, &current_build)?;
            reload_daemon_mounts_once(state_root).map_err(|retry_error| {
                format!(
                    "Could not reload localityd mounts after restarting localityd for the current state schema: {}",
                    retry_error.message
                )
            })
        }
        Err(error) => Err(format!(
            "Could not reload localityd mounts: {}",
            error.message
        )),
    }
}

#[derive(Clone, Debug)]
struct DaemonReloadError {
    message: String,
}

impl DaemonReloadError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn is_unsupported_schema_version(&self) -> bool {
        is_unsupported_schema_version_message(&self.message)
    }
}

fn reload_daemon_mounts_once(state_root: &Path) -> Result<(), DaemonReloadError> {
    match send_request(state_root, &DaemonRequest::ReloadMounts) {
        Ok(response) if response.ok => Ok(()),
        Ok(response) => {
            let message = response
                .error
                .map(|error| format!("{}: {}", error.code, error.message))
                .unwrap_or_else(|| "daemon returned an unknown reload error".to_string());
            Err(DaemonReloadError::new(message))
        }
        Err(error) => Err(DaemonReloadError::new(error.message().to_string())),
    }
}

fn is_unsupported_schema_version_message(message: &str) -> bool {
    message.contains("unsupported schema version") && message.contains("supports up to")
}

fn activate_virtual_projection_mount(
    state_root: &Path,
    mount: &MountConfig,
    wait_for_entities: bool,
) -> Result<(), String> {
    desktop_log(
        "info",
        "file_provider.activate_started",
        format!(
            "activating virtual projection for mount `{}` using {:?}; wait_for_entities={wait_for_entities}",
            mount.mount_id.0, mount.projection
        ),
    );
    if wait_for_entities
        && virtual_projection_waits_for_mount_point_children_before_registration(&mount.projection)
    {
        wait_for_virtual_projection_mount_point_children(state_root, mount)?;
    }
    register_virtual_projection(state_root, mount)?;
    prefetch_virtual_projection_root(state_root, mount)?;
    if wait_for_entities
        && !virtual_projection_waits_for_mount_point_children_before_registration(&mount.projection)
    {
        wait_for_mount_entities(state_root, &mount.mount_id)?;
    }
    ensure_virtual_projection_runtime(state_root, mount)?;
    signal_virtual_projection_refresh(mount);
    recover_virtual_projection_mount_root_if_needed(state_root, mount)?;
    desktop_log(
        "info",
        "file_provider.activate_finished",
        format!(
            "activated virtual projection for mount `{}` using {:?}",
            mount.mount_id.0, mount.projection
        ),
    );
    Ok(())
}

fn recover_virtual_projection_mount_root_if_needed(
    state_root: &Path,
    mount: &MountConfig,
) -> Result<(), String> {
    match mount.projection {
        ProjectionMode::MacosFileProvider => {
            recover_macos_file_provider_mount_root_if_needed(state_root, mount)
        }
        ProjectionMode::LinuxFuse
        | ProjectionMode::PlainFiles
        | ProjectionMode::WindowsCloudFiles => Ok(()),
    }
}

fn virtual_projection_waits_for_mount_point_children_before_registration(
    projection: &ProjectionMode,
) -> bool {
    matches!(
        projection,
        ProjectionMode::MacosFileProvider | ProjectionMode::WindowsCloudFiles
    )
}

#[cfg(target_os = "macos")]
fn recover_macos_file_provider_mount_root_if_needed(
    state_root: &Path,
    mount: &MountConfig,
) -> Result<(), String> {
    let expected_child_count =
        expected_virtual_projection_mount_point_child_count(state_root, mount)?;
    let root = mount_access_root(mount);
    let inspection = evaluate_macos_file_provider_mount_root(&root);
    let Some(mut reason) = macos_file_provider_mount_root_inspection_recovery_reason(
        &root,
        inspection
            .as_ref()
            .map(String::as_str)
            .map_err(String::as_str),
        Some(expected_child_count),
    ) else {
        return Ok(());
    };

    if inspection.is_err() && macos_file_provider_mount_root_is_missing(&reason) {
        match wait_for_macos_file_provider_mount_root_recovery(
            &root,
            expected_child_count,
            MACOS_FILE_PROVIDER_MOUNT_ROOT_APPEAR_TIMEOUT,
        ) {
            Ok(()) => return Ok(()),
            Err(latest_reason) => reason = latest_reason,
        }
    }

    desktop_log(
        "warn",
        "file_provider.recover_started",
        format!(
            "recovering macOS File Provider domain for mount `{}` after activation: {reason}",
            mount.mount_id.0
        ),
    );
    reset_macos_file_provider_domain(&reason)?;
    register_macos_virtual_projection(&mount.mount_id.0, &mount.root.display().to_string())?;
    macos_file_provider_domain_url(localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID)
        .map_err(|error| {
            format!(
                "Could not open macOS File Provider domain after recovery for `{}`: {}",
                mount.mount_id.0,
                error.message()
            )
        })?;
    prefetch_virtual_projection_root(state_root, mount)?;
    ensure_virtual_projection_runtime(state_root, mount)?;
    signal_virtual_projection_refresh(mount);

    wait_for_macos_file_provider_mount_root_recovery(
        &root,
        expected_child_count,
        MACOS_FILE_PROVIDER_MOUNT_ROOT_RECOVERY_TIMEOUT,
    )?;

    desktop_log(
        "info",
        "file_provider.recover_finished",
        format!(
            "recovered macOS File Provider domain for mount `{}`",
            mount.mount_id.0
        ),
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn wait_for_macos_file_provider_mount_root_recovery(
    root: &Path,
    expected_child_count: usize,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;

    loop {
        let reason = match evaluate_macos_file_provider_mount_root(root) {
            Ok(details) => match macos_file_provider_mount_root_recovery_reason(
                root,
                &details,
                Some(expected_child_count),
            ) {
                Some(reason) => reason,
                None => return Ok(()),
            },
            Err(error) => error,
        };

        if Instant::now() >= deadline {
            return Err(format!(
                "macOS File Provider recovery did not produce a healthy mount root `{}`: {reason}",
                root.display()
            ));
        }
        std::thread::sleep(VIRTUAL_PROJECTION_SOURCE_READY_POLL);
    }
}

fn macos_file_provider_mount_root_inspection_recovery_reason(
    root: &Path,
    inspection: Result<&str, &str>,
    expected_child_count: Option<usize>,
) -> Option<String> {
    match inspection {
        Ok(details) => {
            macos_file_provider_mount_root_recovery_reason(root, details, expected_child_count)
        }
        Err(error) => Some(error.to_string()),
    }
}

fn macos_file_provider_mount_root_is_missing(message: &str) -> bool {
    message.contains("NSPOSIXErrorDomain Code=2") || message.contains("Couldn't find a file")
}

#[cfg(target_os = "macos")]
fn expected_virtual_projection_mount_point_child_count(
    state_root: &Path,
    mount: &MountConfig,
) -> Result<usize, String> {
    let report = load_virtual_projection_children_report(
        state_root,
        &mount.mount_id.0,
        &mount_point_identifier(mount),
    )?;
    Ok(report.children.len())
}

#[cfg(target_os = "macos")]
fn evaluate_macos_file_provider_mount_root(root: &Path) -> Result<String, String> {
    let output = Command::new("/usr/bin/fileproviderctl")
        .arg("evaluate")
        .arg(&root)
        .output()
        .map_err(|error| {
            format!(
                "Could not inspect macOS File Provider mount root `{}`: {error}",
                root.display()
            )
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let details = format!("{stdout}{stderr}");
    if !output.status.success() {
        return Err(format!(
            "Could not inspect macOS File Provider mount root `{}`: {}",
            root.display(),
            details.trim()
        ));
    }
    Ok(details)
}

#[cfg(target_os = "macos")]
fn reset_macos_file_provider_domain(reason: &str) -> Result<(), String> {
    run_macos_file_provider_helper("reset", Vec::new())
        .map(|_| ())
        .map_err(|error| {
            format!(
                "Could not reset macOS File Provider domain while recovering from `{reason}`: {}",
                error.message()
            )
        })
}

fn macos_file_provider_mount_root_health_error(root: &Path, details: &str) -> Option<String> {
    if details.contains("uploadingError") || details.contains("NSCocoaErrorDomain Code=3328") {
        return Some(format!(
            "The macOS File Provider mount root `{}` is in a local-upload error state. Run clean-start or reset the Locality File Provider domain, then reconnect Notion.",
            root.display()
        ));
    }
    None
}

fn macos_file_provider_mount_root_recovery_reason(
    root: &Path,
    details: &str,
    expected_child_count: Option<usize>,
) -> Option<String> {
    if let Some(error) = macos_file_provider_mount_root_health_error(root, details) {
        return Some(error);
    }

    let expected_child_count = expected_child_count?;
    let actual_child_count = macos_file_provider_child_item_count(details)?;
    if actual_child_count != expected_child_count {
        return Some(format!(
            "The macOS File Provider mount root `{}` reports {} visible child item{} but Locality has {} current child item{}.",
            root.display(),
            actual_child_count,
            if actual_child_count == 1 { "" } else { "s" },
            expected_child_count,
            if expected_child_count == 1 { "" } else { "s" },
        ));
    }

    None
}

fn macos_file_provider_child_item_count(details: &str) -> Option<usize> {
    let (_, after_label) = details.split_once("childItemCount")?;
    let (_, after_equals) = after_label.split_once('=')?;
    after_equals
        .trim_start()
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

#[cfg(not(target_os = "macos"))]
fn recover_macos_file_provider_mount_root_if_needed(
    _state_root: &Path,
    _mount: &MountConfig,
) -> Result<(), String> {
    Ok(())
}

fn prefetch_virtual_projection_root(state_root: &Path, mount: &MountConfig) -> Result<(), String> {
    for identifier in virtual_projection_prefetch_container_identifiers(mount) {
        prefetch_virtual_projection_container(state_root, &mount.mount_id.0, &identifier)?;
    }
    Ok(())
}

fn virtual_projection_prefetch_container_identifiers(mount: &MountConfig) -> Vec<String> {
    vec![
        ROOT_CONTAINER_IDENTIFIER.to_string(),
        mount_point_identifier(mount),
    ]
}

fn prefetch_virtual_projection_container(
    state_root: &Path,
    mount_id: &str,
    container_identifier: &str,
) -> Result<(), String> {
    let report =
        load_virtual_projection_children_report(state_root, mount_id, container_identifier)?;
    let summary = summarize_virtual_projection_children(&report);
    desktop_log(
        "debug",
        "file_provider.prefetch_children",
        format!(
            "prefetched `{mount_id}:{container_identifier}` with {} child{} ({} content; preview: {})",
            summary.total_children,
            if summary.total_children == 1 {
                ""
            } else {
                "ren"
            },
            summary.content_children,
            summary.preview()
        ),
    );
    Ok(())
}

fn load_virtual_projection_children_report(
    state_root: &Path,
    mount_id: &str,
    container_identifier: &str,
) -> Result<VirtualFsChildrenReport, String> {
    match send_request(
        state_root,
        &DaemonRequest::FileProviderChildren {
            mount_id: mount_id.to_string(),
            container_identifier: container_identifier.to_string(),
        },
    ) {
        Ok(response) if response.ok => {
            let payload = response.payload.ok_or_else(|| {
                format!(
                    "Could not load `{mount_id}:{container_identifier}`: daemon returned no payload."
                )
            })?;
            serde_json::from_value::<VirtualFsChildrenReport>(payload).map_err(|error| {
                format!(
                    "Could not decode `{mount_id}:{container_identifier}` children from localityd: {error}"
                )
            })
        }
        Ok(response) => Err(response
            .error
            .map(|error| {
                format!(
                    "Could not load the top-level Notion folder: {}",
                    error.message
                )
            })
            .unwrap_or_else(|| "Could not load the top-level Notion folder.".to_string())),
        Err(error) => Err(format!(
            "Could not ask localityd to load the top-level Notion folder: {}",
            error.message()
        )),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct VirtualProjectionChildrenSummary {
    total_children: usize,
    content_children: usize,
    content_names: Vec<String>,
}

impl VirtualProjectionChildrenSummary {
    fn preview(&self) -> String {
        if self.content_names.is_empty() {
            return "none".to_string();
        }

        let visible = self
            .content_names
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>();
        let remaining = self.content_names.len().saturating_sub(visible.len());
        if remaining == 0 {
            visible.join(", ")
        } else {
            format!("{}, +{} more", visible.join(", "), remaining)
        }
    }
}

fn summarize_virtual_projection_children(
    report: &VirtualFsChildrenReport,
) -> VirtualProjectionChildrenSummary {
    let content_names = report
        .children
        .iter()
        .filter(|child| !is_virtual_projection_guidance_child(&child.identifier))
        .map(|child| child.filename.clone())
        .collect::<Vec<_>>();

    VirtualProjectionChildrenSummary {
        total_children: report.children.len(),
        content_children: content_names.len(),
        content_names,
    }
}

fn is_virtual_projection_guidance_child(identifier: &str) -> bool {
    identifier.starts_with("guidance:")
}

fn wait_for_virtual_projection_mount_point_children(
    state_root: &Path,
    mount: &MountConfig,
) -> Result<(), String> {
    let mount_id = mount.mount_id.0.as_str();
    let mount_point_container_identifier = mount_point_identifier(mount);
    let started = Instant::now();
    let deadline = started + VIRTUAL_PROJECTION_SOURCE_READY_TIMEOUT;
    let mut attempts = 0_u32;
    let mut next_progress_log = started + VIRTUAL_PROJECTION_SOURCE_READY_LOG_EVERY;
    let mut last_summary = VirtualProjectionChildrenSummary::default();
    let mut last_error: Option<String>;

    desktop_log(
        "info",
        "file_provider.source_ready.wait_started",
        format!(
            "waiting up to {}s for `{mount_id}:{mount_point_container_identifier}` to expose mount-point children before registering virtual projection provider",
            VIRTUAL_PROJECTION_SOURCE_READY_TIMEOUT.as_secs()
        ),
    );

    loop {
        attempts = attempts.saturating_add(1);
        match load_virtual_projection_children_report(
            state_root,
            mount_id,
            &mount_point_container_identifier,
        ) {
            Ok(report) => {
                let summary = summarize_virtual_projection_children(&report);
                last_error = None;
                if summary.content_children > 0 {
                    desktop_log(
                        "info",
                        "file_provider.source_ready.ready",
                        format!(
                            "`{mount_id}:{mount_point_container_identifier}` exposed {} content child{} after {}ms across {attempts} attempt{} ({} total; preview: {})",
                            summary.content_children,
                            if summary.content_children == 1 {
                                ""
                            } else {
                                "ren"
                            },
                            started.elapsed().as_millis(),
                            if attempts == 1 { "" } else { "s" },
                            summary.total_children,
                            summary.preview()
                        ),
                    );
                    return Ok(());
                }
                last_summary = summary;
            }
            Err(error) => {
                last_error = Some(error);
            }
        }

        let now = Instant::now();
        if now >= deadline {
            let diagnostic = last_error
                .as_ref()
                .map(|error| format!("last daemon error: {error}"))
                .unwrap_or_else(|| {
                    format!(
                        "last daemon response had {} child{} but no content children (preview: {})",
                        last_summary.total_children,
                        if last_summary.total_children == 1 {
                            ""
                        } else {
                            "ren"
                        },
                        last_summary.preview()
                    )
                });
            desktop_log(
                "error",
                "file_provider.source_ready.timeout",
                format!(
                    "`{mount_id}:{mount_point_container_identifier}` did not expose mount-point children after {}ms across {attempts} attempt{}; {diagnostic}",
                    started.elapsed().as_millis(),
                    if attempts == 1 { "" } else { "s" }
                ),
            );
            return Err(virtual_projection_source_ready_timeout_message(
                &mount.connector,
                &diagnostic,
            ));
        }

        if now >= next_progress_log {
            let diagnostic = last_error
                .as_ref()
                .map(|error| format!("last daemon error: {error}"))
                .unwrap_or_else(|| {
                    format!(
                        "{} total child{} and {} content child{}",
                        last_summary.total_children,
                        if last_summary.total_children == 1 {
                            ""
                        } else {
                            "ren"
                        },
                        last_summary.content_children,
                        if last_summary.content_children == 1 {
                            ""
                        } else {
                            "ren"
                        }
                    )
                });
            desktop_log(
                "debug",
                "file_provider.source_ready.waiting",
                format!(
                    "still waiting for `{mount_id}:{mount_point_container_identifier}` after {}ms across {attempts} attempt{}; {diagnostic}",
                    started.elapsed().as_millis(),
                    if attempts == 1 { "" } else { "s" }
                ),
            );
            next_progress_log = now + VIRTUAL_PROJECTION_SOURCE_READY_LOG_EVERY;
        }

        std::thread::sleep(VIRTUAL_PROJECTION_SOURCE_READY_POLL);
    }
}

fn virtual_projection_source_ready_timeout_message(connector: &str, diagnostic: &str) -> String {
    let source = connector_label(connector);
    let guidance = if connector == "notion" {
        " Make sure at least one page is selected for Locality access, then try again."
    } else {
        " Confirm the source contains readable items, then retry setup."
    };
    format!(
        "{source} connected, but Locality could not load any files before mounting.{guidance} {diagnostic}"
    )
}

fn ensure_virtual_projection_runtime(state_root: &Path, mount: &MountConfig) -> Result<(), String> {
    match mount.projection {
        ProjectionMode::WindowsCloudFiles => {
            ensure_windows_cloud_files_provider_running(state_root, mount)
        }
        ProjectionMode::MacosFileProvider
        | ProjectionMode::LinuxFuse
        | ProjectionMode::PlainFiles => Ok(()),
    }
}

fn ensure_virtual_projection_runtimes_for_state(state_root: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        ensure_windows_cloud_files_providers_for_state(state_root)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = state_root;
        Ok(())
    }
}

fn ensure_virtual_projection_domains_for_state(state_root: &Path) -> Result<(), String> {
    let mut store = SqliteStateStore::open(state_root.to_path_buf())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Could not load mounts: {error}"))?;
    for mount in &mounts {
        ensure_virtual_projection_domain_available(&mount.projection)?;
    }
    if mounts
        .iter()
        .any(|mount| mount.projection == ProjectionMode::MacosFileProvider)
    {
        repair_macos_file_provider_mount_roots(&mut store)?;
    }
    Ok(())
}

fn ensure_virtual_projection_domain_available(projection: &ProjectionMode) -> Result<(), String> {
    match projection {
        ProjectionMode::MacosFileProvider => ensure_macos_file_provider_domain_available(),
        ProjectionMode::LinuxFuse
        | ProjectionMode::PlainFiles
        | ProjectionMode::WindowsCloudFiles => Ok(()),
    }
}

#[cfg(target_os = "macos")]
fn ensure_macos_file_provider_domain_available() -> Result<(), String> {
    register_macos_file_provider_domain(
        localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID,
        localityd::file_provider::MACOS_FILE_PROVIDER_DISPLAY_NAME,
    )
    .map_err(|error| {
        format!(
            "Could not register macOS File Provider: {}",
            error.message()
        )
    })?;
    macos_file_provider_domain_url(localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID)
        .map(|_| ())
        .map_err(|error| {
            format!(
                "Could not open macOS File Provider domain `{}`: {}",
                localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID,
                error.message()
            )
        })
}

#[cfg(not(target_os = "macos"))]
fn ensure_macos_file_provider_domain_available() -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn repair_macos_file_provider_mount_roots(store: &mut SqliteStateStore) -> Result<(), String> {
    let domain_root =
        macos_file_provider_domain_url(localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID)
            .map_err(|error| {
                format!(
                    "Could not resolve macOS File Provider domain `{}`: {}",
                    localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID,
                    error.message()
                )
            })?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Could not load mounts: {error}"))?;
    for mut mount in mounts {
        if mount.projection != ProjectionMode::MacosFileProvider {
            continue;
        }
        let folder_name = mount
            .root
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                localityd::virtual_fs::source_root_directory_name(&mount.mount_id.0)
            });
        let expected_root = domain_root.join(folder_name);
        if mount.root == expected_root {
            continue;
        }
        mount.root = expected_root;
        store
            .save_mount(mount)
            .map_err(|error| format!("Could not repair macOS File Provider mount root: {error}"))?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn repair_macos_file_provider_mount_roots(_store: &mut SqliteStateStore) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "windows")]
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct WindowsCloudFilesRuntimeProcess {
    pid: u32,
    helper: PathBuf,
}

#[cfg(target_os = "windows")]
fn windows_cloud_files_runtime_processes_from_state(
    state_root: &Path,
) -> Result<Vec<WindowsCloudFilesRuntimeProcess>, String> {
    let lifecycle_dir = state_root.join("cloud-files-lifecycle");
    if !lifecycle_dir.exists() {
        return Ok(Vec::new());
    }

    let mut processes = Vec::new();
    for entry in fs::read_dir(&lifecycle_dir).map_err(|error| {
        format!(
            "Could not inspect Windows Cloud Files lifecycle directory `{}`: {error}",
            lifecycle_dir.display()
        )
    })? {
        let entry = entry
            .map_err(|error| format!("Could not inspect Windows Cloud Files metadata: {error}"))?;
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let bytes = fs::read(entry.path()).map_err(|error| {
            format!(
                "Could not read Windows Cloud Files metadata `{}`: {error}",
                entry.path().display()
            )
        })?;
        let Ok(process) = serde_json::from_slice::<WindowsCloudFilesRuntimeProcess>(&bytes) else {
            continue;
        };
        if process.pid == 0 || process.helper.as_os_str().is_empty() {
            continue;
        }
        processes.push(process);
    }

    processes.sort_by(|left, right| {
        left.pid
            .cmp(&right.pid)
            .then_with(|| left.helper.cmp(&right.helper))
    });
    processes.dedup();
    Ok(processes)
}

#[cfg(target_os = "windows")]
fn stop_windows_cloud_files_runtime_processes_from_state(state_root: &Path) -> Result<(), String> {
    for process in windows_cloud_files_runtime_processes_from_state(state_root)? {
        if !windows_process_is_running(process.pid, &process.helper) {
            continue;
        }
        stop_windows_process(process.pid).map_err(|error| {
            format!(
                "Could not stop Windows Cloud Files provider `{}` (pid {}): {}",
                process.helper.display(),
                process.pid,
                error
            )
        })?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn windows_process_is_running(pid: u32, helper: &Path) -> bool {
    let filter = format!("PID eq {pid}");
    let mut command = Command::new("tasklist");
    command.args(["/FI", &filter, "/FO", "CSV", "/NH"]);
    configure_hidden_windows_command(&mut command);
    let Ok(output) = command.output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = helper
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("locality-cloud-files.exe");
    let pid = pid.to_string();
    stdout.lines().any(|line| {
        let columns = parse_tasklist_csv_line(line);
        let image = columns.first().map(String::as_str).unwrap_or_default();
        let task_pid = columns.get(1).map(String::as_str).unwrap_or_default();
        task_pid == pid
            && (image.eq_ignore_ascii_case(expected)
                || image.eq_ignore_ascii_case("locality-cloud-files.exe"))
    })
}

#[cfg(target_os = "windows")]
fn parse_tasklist_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    while let Some(character) = chars.next() {
        match character {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                current.push('"');
                let _ = chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    fields.push(current.trim().to_string());
    fields
}

#[cfg(target_os = "windows")]
fn stop_windows_process(pid: u32) -> Result<(), String> {
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/T", "/F"]);
    configure_hidden_windows_command(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("Could not stop pid {pid}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    Err(if message.is_empty() {
        format!("taskkill exited with {}", output.status)
    } else {
        message
    })
}

#[cfg(target_os = "windows")]
#[derive(Default)]
struct WindowsCloudFilesProviderSupervisor;

#[cfg(target_os = "windows")]
impl WindowsCloudFilesProviderSupervisor {
    fn ensure_running(&mut self, state_root: &Path, mount: &MountConfig) -> Result<(), String> {
        if mount.projection != ProjectionMode::WindowsCloudFiles {
            return Ok(());
        }

        let report = run_windows_cloud_files_lifecycle(
            state_root,
            mount,
            &connector_label(&mount.connector),
            WindowsCloudFilesLifecycleAction::Start,
        )
        .map_err(|error| {
            format!(
                "Could not start Windows Cloud Files provider `{}`: {}",
                mount.mount_id.0,
                error.message()
            )
        })?;
        if let Some(message) = report
            .helper_report
            .get("message")
            .and_then(serde_json::Value::as_str)
        {
            eprintln!("{message}");
        }
        Ok(())
    }

    fn stop_all(&mut self, state_root: &Path) {
        for mount in windows_cloud_files_mounts(state_root) {
            if let Err(error) = self.stop_mount(state_root, &mount) {
                eprintln!(
                    "loc desktop could not stop Windows Cloud Files provider `{}`: {error}",
                    mount.mount_id.0
                );
            }
        }
    }

    fn stop_mount(&mut self, state_root: &Path, mount: &MountConfig) -> Result<(), String> {
        run_windows_cloud_files_lifecycle(
            state_root,
            mount,
            &connector_label(&mount.connector),
            WindowsCloudFilesLifecycleAction::Stop,
        )
        .map(|_| ())
        .map_err(|error| error.message())
    }
}

#[cfg(target_os = "windows")]
fn windows_cloud_files_provider_supervisor() -> &'static Mutex<WindowsCloudFilesProviderSupervisor>
{
    WINDOWS_CLOUD_FILES_PROVIDER_SUPERVISOR
        .get_or_init(|| Mutex::new(WindowsCloudFilesProviderSupervisor::default()))
}

#[cfg(target_os = "windows")]
fn ensure_windows_cloud_files_provider_running(
    state_root: &Path,
    mount: &MountConfig,
) -> Result<(), String> {
    windows_cloud_files_provider_supervisor()
        .lock()
        .map_err(|_| "Windows Cloud Files provider supervisor lock was poisoned".to_string())?
        .ensure_running(state_root, mount)
}

#[cfg(not(target_os = "windows"))]
fn ensure_windows_cloud_files_provider_running(
    _state_root: &Path,
    _mount: &MountConfig,
) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "windows")]
fn ensure_windows_cloud_files_providers_for_state(state_root: &Path) -> Result<(), String> {
    let cloud_mounts = load_windows_cloud_files_mounts(state_root)?;

    if cloud_mounts.is_empty() {
        return Ok(());
    }

    ensure_daemon_running(state_root)?;
    reload_daemon_mounts(state_root)?;
    let mut mounts_by_root = BTreeMap::new();
    for mount in cloud_mounts {
        mounts_by_root
            .entry(localityd::virtual_fs::virtual_projection_root(&mount))
            .or_insert(mount);
    }
    for mount in mounts_by_root.values() {
        ensure_windows_cloud_files_provider_running(state_root, mount)?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn load_windows_cloud_files_mounts(state_root: &Path) -> Result<Vec<MountConfig>, String> {
    let store = SqliteStateStore::open(state_root.to_path_buf())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    Ok(store
        .load_mounts()
        .map_err(|error| format!("Could not load mounts: {error}"))?
        .into_iter()
        .filter(|mount| mount.projection == ProjectionMode::WindowsCloudFiles)
        .collect::<Vec<_>>())
}

#[cfg(target_os = "windows")]
fn windows_cloud_files_mounts(state_root: &Path) -> Vec<MountConfig> {
    load_windows_cloud_files_mounts(state_root).unwrap_or_else(|error| {
        eprintln!("loc desktop could not load Windows Cloud Files mounts: {error}");
        Vec::new()
    })
}

fn start_windows_cloud_files_provider_supervisor() {
    #[cfg(target_os = "windows")]
    {
        std::thread::spawn(|| {
            let state_root = default_state_root();
            loop {
                if LOCAL_STATE_RESET_IN_PROGRESS.load(Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    continue;
                }
                if let Err(error) = ensure_virtual_projection_runtimes_for_state(&state_root) {
                    eprintln!(
                        "loc desktop could not supervise Windows Cloud Files provider: {error}"
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        });
    }
}

fn stop_windows_cloud_files_provider_supervisor(state_root: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        if let Some(supervisor) = WINDOWS_CLOUD_FILES_PROVIDER_SUPERVISOR.get() {
            match supervisor.lock() {
                Ok(mut supervisor) => supervisor.stop_all(state_root),
                Err(_) => {
                    return Err(
                        "Could not stop Windows Cloud Files providers: lock poisoned".to_string(),
                    );
                }
            }
        }
        stop_windows_cloud_files_runtime_processes_from_state(state_root)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = state_root;
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn stop_windows_cloud_files_provider_supervisor_for_shutdown() {
    if let Err(error) = stop_windows_cloud_files_provider_supervisor(&default_state_root()) {
        eprintln!("loc desktop could not stop Windows Cloud Files providers: {error}");
    }
}

#[cfg(not(target_os = "windows"))]
fn stop_windows_cloud_files_provider_supervisor_for_shutdown() {}

fn daemon_is_ready(state_root: &Path) -> bool {
    matches!(
        send_request(state_root, &DaemonRequest::Ping),
        Ok(response) if response.ok
    )
}

fn running_daemon_build(state_root: &Path) -> Option<DaemonBuildInfo> {
    let response = send_request(state_root, &DaemonRequest::Status).ok()?;
    if !response.ok {
        return None;
    }
    let payload = response.payload?;
    serde_json::from_value::<DaemonStatusReport>(payload)
        .ok()
        .map(|report| report.build)
}

fn signal_virtual_projection_refresh(mount: &MountConfig) {
    for identifier in virtual_projection_refresh_signal_identifiers(mount) {
        if let Err(error) = signal_virtual_projection_container(mount, &identifier) {
            desktop_log(
                "warn",
                "file_provider.signal_failed",
                format!(
                    "could not signal {}:{} refresh: {error}",
                    mount.mount_id.0, identifier
                ),
            );
        }
    }
}

fn virtual_projection_refresh_signal_identifiers(mount: &MountConfig) -> Vec<String> {
    if mount.projection == ProjectionMode::MacosFileProvider {
        return vec![
            ROOT_CONTAINER_IDENTIFIER.to_string(),
            mount_point_identifier(mount),
            "working-set".to_string(),
        ];
    }
    vec![
        ROOT_CONTAINER_IDENTIFIER.to_string(),
        mount_point_identifier(mount),
    ]
}

fn signal_virtual_projection_container(
    mount: &MountConfig,
    container_identifier: &str,
) -> Result<(), String> {
    match mount.projection {
        ProjectionMode::MacosFileProvider => {
            refresh_macos_virtual_projection(&mount.mount_id.0, container_identifier)
        }
        ProjectionMode::LinuxFuse
        | ProjectionMode::PlainFiles
        | ProjectionMode::WindowsCloudFiles => Ok(()),
    }
}

#[cfg(target_os = "macos")]
fn refresh_macos_virtual_projection(mount_id: &str, identifier: &str) -> Result<(), String> {
    if identifier == "working-set" {
        return signal_macos_virtual_projection(mount_id, identifier);
    }

    reimport_macos_virtual_projection(mount_id, identifier)
        .or_else(|_| signal_macos_virtual_projection(mount_id, identifier))
}

#[cfg(target_os = "macos")]
fn signal_macos_virtual_projection(mount_id: &str, identifier: &str) -> Result<(), String> {
    let provider_identifier = if identifier == "working-set" {
        "working-set".to_string()
    } else {
        daemon_file_provider::macos_file_provider_item_identifier(mount_id, identifier)
    };
    run_macos_file_provider_refresh_action("signal", &provider_identifier).map_err(
        |signal_error| {
            format!(
                "Could not refresh macOS File Provider for `{identifier}`: signal failed: {signal_error}"
            )
        },
    )
}

#[cfg(target_os = "macos")]
fn reimport_macos_virtual_projection(mount_id: &str, identifier: &str) -> Result<(), String> {
    let provider_identifier =
        daemon_file_provider::macos_file_provider_item_identifier(mount_id, identifier);
    run_macos_file_provider_refresh_action("reimport", &provider_identifier).map_err(
        |reimport_error| {
            format!(
                "Could not refresh macOS File Provider for `{identifier}`: reimport failed: {reimport_error}"
            )
        },
    )
}

#[cfg(target_os = "macos")]
fn run_macos_file_provider_refresh_action(
    action: &str,
    provider_identifier: &str,
) -> Result<(), String> {
    run_macos_file_provider_helper(
        action,
        vec![
            "--mount-id".to_string(),
            localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID.to_string(),
            "--identifier".to_string(),
            provider_identifier.to_string(),
        ],
    )
    .map(|_| ())
    .map_err(|error| error.message().to_string())
}

#[cfg(not(target_os = "macos"))]
fn refresh_macos_virtual_projection(_mount_id: &str, _identifier: &str) -> Result<(), String> {
    Ok(())
}

fn register_virtual_projection(state_root: &Path, mount: &MountConfig) -> Result<(), String> {
    match mount.projection {
        ProjectionMode::MacosFileProvider => {
            register_macos_virtual_projection(&mount.mount_id.0, &mount.root.display().to_string())
        }
        ProjectionMode::LinuxFuse => register_linux_virtual_projection(state_root, mount),
        ProjectionMode::PlainFiles => Ok(()),
        ProjectionMode::WindowsCloudFiles => register_windows_virtual_projection(state_root, mount),
    }
}

#[cfg(target_os = "macos")]
fn register_macos_virtual_projection(_mount_id: &str, _root: &str) -> Result<(), String> {
    register_macos_file_provider_domain(
        localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID,
        localityd::file_provider::MACOS_FILE_PROVIDER_DISPLAY_NAME,
    )
    .map(|_| ())
    .map_err(|error| {
        format!(
            "Could not register macOS File Provider: {}",
            error.message()
        )
    })
}

#[cfg(not(target_os = "macos"))]
fn register_macos_virtual_projection(_mount_id: &str, _root: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn register_linux_virtual_projection(state_root: &Path, mount: &MountConfig) -> Result<(), String> {
    loc_cli::file_provider::register_linux_fuse_mount(state_root, mount)
        .map(|_| ())
        .map_err(|error| format!("Could not register Linux FUSE mount: {}", error.message()))
}

#[cfg(not(target_os = "linux"))]
fn register_linux_virtual_projection(
    _state_root: &Path,
    mount: &MountConfig,
) -> Result<(), String> {
    Err(format!(
        "Linux FUSE mounts can only be registered on Linux; mount `{}` cannot be registered here.",
        mount.mount_id.0
    ))
}

#[cfg(target_os = "windows")]
fn register_windows_virtual_projection(
    state_root: &Path,
    mount: &MountConfig,
) -> Result<(), String> {
    register_windows_cloud_files_sync_root(state_root, mount, &connector_label(&mount.connector))
        .map(|_| ())
        .map_err(|error| {
            format!(
                "Could not register Windows Cloud Files sync root: {}",
                error.message()
            )
        })
}

#[cfg(not(target_os = "windows"))]
fn register_windows_virtual_projection(
    _state_root: &Path,
    mount: &MountConfig,
) -> Result<(), String> {
    Err(format!(
        "Windows Cloud Files mounts can only be registered on Windows; mount `{}` cannot be registered here.",
        mount.mount_id.0
    ))
}

fn open_virtual_mount_or_path(path: &Path) -> Result<(), String> {
    if let Some(mount) = virtual_mount_for_path(path)
        && mount.projection.uses_virtual_filesystem()
    {
        return open_virtual_projection(&mount);
    }

    open_in_file_manager(path)
}

fn virtual_mount_for_path(path: &Path) -> Option<MountConfig> {
    let store = SqliteStateStore::open(default_state_root()).ok()?;
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mounts = store.load_mounts().ok()?;
    daemon_file_provider::find_mount_for_path(&mounts, &target).map(|(mount, _)| mount.clone())
}

fn open_virtual_projection(mount: &MountConfig) -> Result<(), String> {
    match mount.projection {
        ProjectionMode::MacosFileProvider => open_macos_virtual_projection(mount),
        ProjectionMode::LinuxFuse => open_in_file_manager(&mount_access_root(mount)),
        ProjectionMode::PlainFiles => open_in_file_manager(&mount.root),
        ProjectionMode::WindowsCloudFiles => open_windows_virtual_projection(mount),
    }
}

#[cfg(target_os = "macos")]
fn open_macos_virtual_projection(mount: &MountConfig) -> Result<(), String> {
    match macos_file_provider_domain_url(localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID) {
        Ok(provider_root) => {
            let mount_point_root = provider_root.join(mount_point_directory_name(mount));
            open_in_file_manager(&mount_point_root)
        }
        Err(error) => {
            let first_error = error.message();
            desktop_log(
                "warn",
                "file_provider.open_domain_failed",
                format!(
                    "could not open macOS File Provider domain `{}`: {first_error}",
                    localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID
                ),
            );

            if let Err(error) = register_macos_virtual_projection(
                &mount.mount_id.0,
                &mount.root.display().to_string(),
            ) {
                desktop_log(
                    "warn",
                    "file_provider.reregister_failed",
                    format!("could not re-register macOS File Provider domain: {error}"),
                );
            } else if let Ok(provider_root) = macos_file_provider_domain_url(
                localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID,
            ) {
                let mount_point_root = provider_root.join(mount_point_directory_name(mount));
                if open_in_file_manager(&mount_point_root).is_ok() {
                    return Ok(());
                }
            }

            open_in_file_manager(&mount.root).map_err(|fallback_error| {
                format!(
                    "Could not open macOS File Provider mount ({first_error}). Also could not open fallback folder `{}`: {fallback_error}",
                    mount.root.display()
                )
            })
        }
    }
}

#[cfg(target_os = "windows")]
fn open_windows_virtual_projection(mount: &MountConfig) -> Result<(), String> {
    match open_windows_cloud_files_sync_root(mount) {
        Ok(_) => {
            let mount = mount.clone();
            std::thread::spawn(move || {
                let state_root = default_state_root();
                if let Err(error) = ensure_windows_cloud_files_provider_running(&state_root, &mount)
                {
                    eprintln!(
                        "loc desktop could not prepare Windows Cloud Files provider while opening `{}`: {error}",
                        mount.mount_id.0
                    );
                }
            });
            Ok(())
        }
        Err(open_error) => {
            let first_error = open_error.message();
            let state_root = default_state_root();
            ensure_windows_cloud_files_provider_running(&state_root, mount)?;
            open_windows_cloud_files_sync_root(mount)
                .map(|_| ())
                .map_err(|retry_error| {
                    format!(
                        "Could not open Windows Cloud Files sync root: {}. Initial open failed with: {first_error}",
                        retry_error.message()
                    )
                })
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn open_windows_virtual_projection(mount: &MountConfig) -> Result<(), String> {
    Err(format!(
        "Windows Cloud Files mounts can only be opened on Windows; mount `{}` cannot be opened here.",
        mount.mount_id.0
    ))
}

#[cfg(not(target_os = "macos"))]
fn open_macos_virtual_projection(_mount: &MountConfig) -> Result<(), String> {
    Err("macOS File Provider mounts can only be opened on macOS.".to_string())
}

fn connect_notion_with_broker(state_root: PathBuf, open_browser: bool) -> Result<String, String> {
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let credentials = open_credential_store(&state_root);
    let broker_url = env_first(&[
        "LOCALITY_NOTION_OAUTH_BROKER_URL",
        "LOCALITY_AUTH_BROKER_URL",
    ])
    .unwrap_or_else(|| DEFAULT_LOCALITY_NOTION_OAUTH_BROKER_URL.to_string());
    let redirect_uri = env_first(&[
        "LOCALITY_NOTION_OAUTH_REDIRECT_URI",
        "NOTION_OAUTH_REDIRECT_URI",
    ])
    .unwrap_or_else(|| "http://localhost:8757/oauth/notion/callback".to_string());
    let broker = HttpNotionOAuthBrokerClient::new(broker_url.clone());
    let start = broker
        .start(&NotionOAuthBrokerStart {
            redirect_uri: redirect_uri.clone(),
        })
        .map_err(|error| format!("Could not start Notion OAuth broker flow: {error}"))?;
    let authorization_url = start.normalized_authorization_url();
    set_notion_login_link(authorization_url.clone());
    let authorization = run_local_oauth_authorization(
        "Notion",
        &authorization_url,
        &start.redirect_uri,
        &start.state,
        !open_browser,
        true,
    )
    .map_err(|error| error.message)?;
    let connection_id = reusable_notion_connection_id(&store);
    let previous_connection = connection_id
        .as_ref()
        .and_then(|connection_id| store.get_connection(connection_id).ok().flatten());
    let options = BrokerOAuthConnectOptions {
        connection_id,
        broker_url,
        client_id: start.client_id,
        session: start.session,
        state: start.state,
        code: authorization.code,
        redirect_uri: start.redirect_uri,
    };

    let report =
        run_connect_notion_broker_oauth(&mut store, credentials.as_ref(), options, &broker)
            .map_err(|error| error.message())?;
    let refresh_message = refresh_notion_mount_after_connect(
        &state_root,
        &mut store,
        ConnectionId::new(report.connection_id.clone()),
        previous_connection.as_ref(),
    )?;

    let connected_message = match report.workspace_name.or(report.account_label) {
        Some(label) if !label.is_empty() => format!("Connected Notion workspace {label}."),
        _ => "Connected Notion workspace.".to_string(),
    };
    Ok(format!("{connected_message} {refresh_message}"))
}

fn connect_google_docs_with_broker(
    state_root: PathBuf,
    open_browser: bool,
) -> Result<String, String> {
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let credentials = open_credential_store(&state_root);
    let broker_url = env_first(&[
        "LOCALITY_GOOGLE_DOCS_OAUTH_BROKER_URL",
        "LOCALITY_AUTH_BROKER_URL",
    ])
    .unwrap_or_else(|| DEFAULT_GOOGLE_DOCS_OAUTH_BROKER_URL.to_string());
    let redirect_uri = env_first(&[
        "LOCALITY_GOOGLE_DOCS_OAUTH_REDIRECT_URI",
        "GOOGLE_DOCS_OAUTH_REDIRECT_URI",
    ])
    .unwrap_or_else(|| DEFAULT_GOOGLE_DOCS_OAUTH_REDIRECT_URI.to_string());
    let broker = HttpGoogleDocsOAuthBrokerClient::new(broker_url.clone());
    let start = broker
        .start(&OAuthBrokerStart {
            connector: GOOGLE_DOCS_CONNECTOR_ID.to_string(),
            redirect_uri,
        })
        .map_err(|error| format!("Could not start Google Docs OAuth broker flow: {error}"))?;
    let authorization = run_local_oauth_authorization(
        "Google Docs",
        &start.authorization_url,
        &start.redirect_uri,
        &start.state,
        !open_browser,
        true,
    )
    .map_err(|error| error.message)?;
    let options = GoogleDocsBrokerOAuthConnectOptions {
        connection_id: None,
        broker_url,
        client_id: start.client_id,
        session: start.session,
        state: start.state,
        code: authorization.code,
        redirect_uri: start.redirect_uri,
    };

    let report =
        run_connect_google_docs_broker_oauth(&mut store, credentials.as_ref(), options, &broker)
            .map_err(|error| error.message())?;
    let connected_message = match report.workspace_name.or(report.account_label) {
        Some(label) if !label.is_empty() => format!("Connected Google Docs account {label}."),
        _ => "Connected Google Docs.".to_string(),
    };
    Ok(format!(
        "{connected_message} Create a Google Docs source folder to mount a Drive workspace."
    ))
}

fn connect_gmail_with_broker(state_root: PathBuf, open_browser: bool) -> Result<String, String> {
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let credentials = open_credential_store(&state_root);
    let broker_url = env_first(&[
        "LOCALITY_GMAIL_OAUTH_BROKER_URL",
        "LOCALITY_AUTH_BROKER_URL",
    ])
    .unwrap_or_else(|| DEFAULT_GMAIL_OAUTH_BROKER_URL.to_string());
    let redirect_uri = env_first(&[
        "LOCALITY_GMAIL_OAUTH_REDIRECT_URI",
        "GMAIL_OAUTH_REDIRECT_URI",
    ])
    .unwrap_or_else(|| DEFAULT_GMAIL_OAUTH_REDIRECT_URI.to_string());
    let broker = HttpGmailOAuthBrokerClient::new(broker_url.clone());
    let start = broker
        .start(&OAuthBrokerStart {
            connector: GMAIL_CONNECTOR_ID.to_string(),
            redirect_uri,
        })
        .map_err(|error| format!("Could not start Gmail OAuth broker flow: {error}"))?;
    let authorization = run_local_oauth_authorization(
        "Gmail",
        &start.authorization_url,
        &start.redirect_uri,
        &start.state,
        !open_browser,
        true,
    )
    .map_err(|error| error.message)?;
    let options = GmailBrokerOAuthConnectOptions {
        connection_id: None,
        broker_url,
        client_id: start.client_id,
        session: start.session,
        state: start.state,
        code: authorization.code,
        redirect_uri: start.redirect_uri,
    };

    let report = run_connect_gmail_broker_oauth(&mut store, credentials.as_ref(), options, &broker)
        .map_err(|error| error.message())?;
    let connected_message = match report.workspace_name.or(report.account_label) {
        Some(label) if !label.is_empty() => format!("Connected Gmail account {label}."),
        _ => "Connected Gmail.".to_string(),
    };
    Ok(format!(
        "{connected_message} Create a Gmail source folder to mount inbox, sent, and draft mailboxes."
    ))
}

fn notion_login_link_slot() -> &'static Mutex<Option<String>> {
    NOTION_LOGIN_LINK.get_or_init(|| Mutex::new(None))
}

fn set_notion_login_link(url: String) {
    if let Ok(mut link) = notion_login_link_slot().lock() {
        *link = Some(url);
    }
}

fn clear_notion_login_link() {
    if let Ok(mut link) = notion_login_link_slot().lock() {
        *link = None;
    }
}

fn refresh_notion_mount_after_connect(
    state_root: &Path,
    store: &mut SqliteStateStore,
    connection_id: ConnectionId,
    previous_connection: Option<&ConnectionRecord>,
) -> Result<String, String> {
    let Some(mut mount) = store
        .load_mounts()
        .map_err(|error| format!("Could not load mounts: {error}"))?
        .into_iter()
        .find(|mount| mount.mount_id.0 == "notion-main" && mount.connector == "notion")
    else {
        return Ok("Create a Notion mount point for the newly connected workspace.".to_string());
    };

    let next_connection = store
        .get_connection(&connection_id)
        .map_err(|error| format!("Could not load connected Notion metadata: {error}"))?;
    let connection_changed =
        connection_metadata_changed(previous_connection, next_connection.as_ref());
    let has_unfinished_journals = mount_has_unfinished_journals(store, &mount.mount_id)?;
    let preserved = if mount_has_pending_local_changes(store, state_root, &mount.mount_id)? {
        preserve_mount_pending_local_changes(store, state_root, &mount.mount_id)?
    } else {
        None
    };

    mount.connection_id = Some(connection_id);
    ensure_virtual_projection_domain_available(&mount.projection)?;
    store
        .save_mount(mount.clone())
        .map_err(|error| format!("Could not update Notion mount connection: {error}"))?;

    ensure_daemon_running(state_root)?;
    if has_unfinished_journals {
        reload_daemon_mounts(state_root)?;
        return Ok(
            "Locality updated the connection metadata, but kept the current mount cache because a push is still in progress. Try Change Notion Access again after it finishes."
                .to_string(),
        );
    }

    clear_mount_cached_projection(store, state_root, &mount.mount_id)?;
    let credentials = open_credential_store(state_root);
    let connector = resolve_source_for_mount_id(store, credentials.as_ref(), &mount.mount_id)
        .map_err(|error| {
            format!(
                "Could not prepare the refreshed Notion access: {}",
                error.message()
            )
        })?;
    refresh_mount_root_after_access_change(store, &connector, state_root, &mount)?;
    reload_daemon_mounts(state_root)?;

    let projection_warning = if mount.projection.uses_virtual_filesystem() {
        match activate_virtual_projection_mount(state_root, &mount, true) {
            Ok(()) => None,
            Err(error) if recoverable_macos_file_provider_activation_error(&error) => {
                Some(format!(
                    "The Notion connection was updated, but macOS File Provider needs repair before Finder can show the folder: {error}"
                ))
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };

    let mut message = if let Some(preserved) = preserved {
        format!(
            "Locality preserved {} local Notion change{} at `{}` and refreshed the mounted folder for the latest Notion access.",
            preserved.count,
            if preserved.count == 1 { "" } else { "s" },
            preserved.directory.display()
        )
    } else if connection_changed {
        "Locality refreshed the mounted folder for the newly connected workspace.".to_string()
    } else {
        "Locality refreshed the mounted folder for the latest Notion access.".to_string()
    };

    if let Some(warning) = projection_warning {
        message.push(' ');
        message.push_str(&warning);
    }
    Ok(message)
}

fn refresh_mount_root_after_access_change<Source>(
    store: &mut SqliteStateStore,
    source: &Source,
    state_root: &Path,
    mount: &MountConfig,
) -> Result<PullReport, String>
where
    Source: SourceAdapter + Clone,
{
    run_pull_with_state_root(store, source, mount.root.clone(), Some(state_root)).map_err(|error| {
        format!(
            "Could not populate the refreshed Notion access for `{}`: {}",
            mount.root.display(),
            error.message()
        )
    })
}

fn connection_metadata_changed(
    previous: Option<&ConnectionRecord>,
    next: Option<&ConnectionRecord>,
) -> bool {
    previous.map(connection_metadata_key) != next.map(connection_metadata_key)
}

fn recoverable_macos_file_provider_activation_error(message: &str) -> bool {
    message.contains("The application cannot be used right now")
        || message.contains("locality-file-providerctl was not found")
        || message.contains("registered but not enabled")
        || message.contains("did not return a CloudStorage URL")
        || message.contains("macOS has not created")
}

fn workspace_mount_onboarding_report(
    state: MacosWorkspaceMountOnboardingState,
    message: impl Into<String>,
    primary_action: WorkspaceMountOnboardingPrimaryAction,
    launch_strategy: WorkspaceMountOnboardingLaunchStrategy,
) -> WorkspaceMountOnboardingReport {
    WorkspaceMountOnboardingReport {
        state: state.as_str().to_string(),
        message: message.into(),
        primary_action: primary_action.as_str().to_string(),
        launch_strategy: launch_strategy.as_str().to_string(),
    }
}

fn workspace_mount_onboarding_curated_message(
    state: MacosWorkspaceMountOnboardingState,
) -> Option<&'static str> {
    match state {
        MacosWorkspaceMountOnboardingState::ApprovalRequired => {
            Some("In Finder, click Enable for Locality. Locality will continue automatically.")
        }
        MacosWorkspaceMountOnboardingState::WaitingForCloudStorageRoot => {
            Some("Locality is still waiting for the CloudStorage folder to appear.")
        }
        _ => None,
    }
}

fn workspace_mount_onboarding_should_refresh_surfaces(
    report: &WorkspaceMountOnboardingReport,
) -> bool {
    report.state == MacosWorkspaceMountOnboardingState::Created.as_str()
}

#[cfg(target_os = "macos")]
fn macos_workspace_mount_onboarding_state(
    message: &str,
    user_enabled: bool,
) -> Option<MacosWorkspaceMountOnboardingState> {
    if message.contains("registered but not enabled") {
        return Some(MacosWorkspaceMountOnboardingState::ApprovalRequired);
    }
    if user_enabled
        && (message.contains("did not return a CloudStorage URL")
            || message.contains("macOS has not created"))
    {
        return Some(MacosWorkspaceMountOnboardingState::WaitingForCloudStorageRoot);
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn macos_workspace_mount_onboarding_state(
    _message: &str,
    _user_enabled: bool,
) -> Option<MacosWorkspaceMountOnboardingState> {
    None
}

fn classify_workspace_mount_onboarding_failure(message: &str) -> WorkspaceMountOnboardingReport {
    #[cfg(target_os = "macos")]
    {
        if recoverable_macos_file_provider_activation_error(message) {
            let user_enabled = macos_workspace_mount_domain_user_enabled().unwrap_or(false);
            if let Some(state) = macos_workspace_mount_onboarding_state(message, user_enabled) {
                let primary_action = match state {
                    MacosWorkspaceMountOnboardingState::ApprovalRequired => {
                        WorkspaceMountOnboardingPrimaryAction::AllowInMacos
                    }
                    MacosWorkspaceMountOnboardingState::WaitingForCloudStorageRoot => {
                        WorkspaceMountOnboardingPrimaryAction::CheckAgain
                    }
                    _ => WorkspaceMountOnboardingPrimaryAction::RetrySetup,
                };
                return workspace_mount_onboarding_report(
                    state,
                    workspace_mount_onboarding_curated_message(state).unwrap_or(message),
                    primary_action,
                    WorkspaceMountOnboardingLaunchStrategy::InstructionsOnly,
                );
            }
        }
    }

    workspace_mount_onboarding_report(
        MacosWorkspaceMountOnboardingState::Failed,
        message,
        WorkspaceMountOnboardingPrimaryAction::RetrySetup,
        WorkspaceMountOnboardingLaunchStrategy::None,
    )
}

#[cfg(target_os = "macos")]
fn macos_workspace_mount_domain_user_enabled() -> Result<bool, String> {
    let report =
        run_macos_file_provider_helper("list", Vec::new()).map_err(|error| error.message())?;
    Ok(report
        .helper_report
        .get("domains")
        .and_then(serde_json::Value::as_array)
        .and_then(|domains| {
            domains.iter().find(|domain| {
                domain.get("identifier").and_then(serde_json::Value::as_str)
                    == Some(localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID)
            })
        })
        .and_then(|domain| domain.get("userEnabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false))
}

fn classify_macos_file_provider_enablement(
    user_enabled: Option<bool>,
    fallback_path: Option<PathBuf>,
    resolved_root: Result<Option<PathBuf>, String>,
) -> FileProviderEnablementReport {
    let path = |value: Option<PathBuf>| value.map(|path| path.display().to_string());
    match user_enabled {
        None => FileProviderEnablementReport {
            state: "not_registered".to_string(),
            message: "Locality is preparing the Finder location.".to_string(),
            path: path(fallback_path),
        },
        Some(false) => FileProviderEnablementReport {
            state: "needs_finder_enable".to_string(),
            message: "In Finder, click Enable for Locality.".to_string(),
            path: path(fallback_path),
        },
        Some(true) => match resolved_root {
            Ok(Some(root)) => FileProviderEnablementReport {
                state: "ready".to_string(),
                message: "Locality is enabled in Finder.".to_string(),
                path: path(Some(root)),
            },
            Ok(None) => FileProviderEnablementReport {
                state: "waiting_for_root".to_string(),
                message: "Finishing the Locality folder setup.".to_string(),
                path: path(fallback_path),
            },
            Err(message) => FileProviderEnablementReport {
                state: "unavailable".to_string(),
                message,
                path: path(fallback_path),
            },
        },
    }
}

fn macos_file_provider_domain_status(
    report: &serde_json::Value,
) -> (Option<bool>, Option<PathBuf>) {
    let domain = report
        .get("domains")
        .and_then(serde_json::Value::as_array)
        .and_then(|domains| {
            domains.iter().find(|domain| {
                domain.get("identifier").and_then(serde_json::Value::as_str)
                    == Some(localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID)
            })
        });
    let user_enabled = domain
        .and_then(|domain| domain.get("userEnabled"))
        .and_then(serde_json::Value::as_bool);
    let path = domain
        .and_then(|domain| domain.get("url"))
        .and_then(serde_json::Value::as_str)
        .filter(|url| !url.is_empty())
        .map(PathBuf::from);
    (user_enabled, path)
}

#[cfg(target_os = "macos")]
fn macos_file_provider_enablement_status_blocking() -> FileProviderEnablementReport {
    let provider_roots = macos_file_provider_cloud_storage_roots();
    let report = match run_macos_file_provider_helper("list", Vec::new()) {
        Ok(report) => report,
        Err(error) => {
            return FileProviderEnablementReport {
                state: "unavailable".to_string(),
                message: error.message(),
                path: provider_roots
                    .first()
                    .map(|path| path.display().to_string()),
            };
        }
    };
    let (user_enabled, reported_path) = macos_file_provider_domain_status(&report.helper_report);
    let fallback_path = reported_path.or_else(|| provider_roots.first().cloned());

    let resolved_root = if user_enabled == Some(true) {
        match macos_file_provider_domain_url(
            localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID,
        ) {
            Ok(root) => Ok(Some(root)),
            Err(error) if recoverable_macos_file_provider_activation_error(&error.message()) => {
                Ok(None)
            }
            Err(error) => Err(error.message()),
        }
    } else {
        Ok(None)
    };
    classify_macos_file_provider_enablement(user_enabled, fallback_path, resolved_root)
}

#[cfg(not(target_os = "macos"))]
fn macos_file_provider_enablement_status_blocking() -> FileProviderEnablementReport {
    FileProviderEnablementReport {
        state: "unavailable".to_string(),
        message: "File Provider enablement is only available on macOS.".to_string(),
        path: None,
    }
}

fn macos_file_provider_reveal_path(
    domain_path: Option<&Path>,
    candidates: &[PathBuf],
) -> Option<PathBuf> {
    domain_path
        .into_iter()
        .chain(candidates.iter().map(PathBuf::as_path))
        .find_map(|candidate| {
            candidate
                .ancestors()
                .find(|ancestor| ancestor.is_dir())
                .map(Path::to_path_buf)
        })
}

#[cfg(target_os = "macos")]
fn reveal_file_provider_enablement_blocking() -> Result<String, String> {
    let report = macos_file_provider_enablement_status_blocking();
    let domain_path = report.path.as_deref().map(Path::new);
    let path =
        macos_file_provider_reveal_path(domain_path, &macos_file_provider_cloud_storage_roots())
            .ok_or_else(|| {
                "Could not find the Locality location or its CloudStorage parent.".to_string()
            })?;
    open_in_file_manager(&path)?;
    Ok(format!("Opened {} in Finder.", path.display()))
}

#[cfg(not(target_os = "macos"))]
fn reveal_file_provider_enablement_blocking() -> Result<String, String> {
    Err("File Provider enablement is only available on macOS.".to_string())
}

#[cfg(not(target_os = "macos"))]
fn macos_workspace_mount_domain_user_enabled() -> Result<bool, String> {
    Ok(false)
}

#[cfg(target_os = "macos")]
fn macos_file_provider_approval_surface_path(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|path| path.exists()).cloned()
}

#[cfg(not(target_os = "macos"))]
fn macos_file_provider_approval_surface_path(_candidates: &[PathBuf]) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
fn launch_macos_file_provider_approval_surface() -> WorkspaceMountOnboardingLaunchStrategy {
    if let Some(path) =
        macos_file_provider_approval_surface_path(&macos_file_provider_cloud_storage_roots())
    {
        if open_in_file_manager(&path).is_ok() {
            return WorkspaceMountOnboardingLaunchStrategy::OpenFinder;
        }
    }
    WorkspaceMountOnboardingLaunchStrategy::InstructionsOnly
}

#[cfg(not(target_os = "macos"))]
fn launch_macos_file_provider_approval_surface() -> WorkspaceMountOnboardingLaunchStrategy {
    WorkspaceMountOnboardingLaunchStrategy::None
}

fn connection_metadata_key(
    connection: &ConnectionRecord,
) -> (&str, Option<&str>, Option<&str>, Option<&str>) {
    (
        connection.connector.as_str(),
        connection.workspace_id.as_deref(),
        connection.workspace_name.as_deref(),
        connection.account_label.as_deref(),
    )
}

fn prepare_existing_workspace_mount_for_remount(
    store: &mut SqliteStateStore,
    state_root: &Path,
    mount_id: &MountId,
) -> Result<Option<PreservedLocalChanges>, String> {
    let Some(mount) = store
        .get_mount(mount_id)
        .map_err(|error| format!("Could not inspect existing source mount: {error}"))?
    else {
        return Ok(None);
    };

    if mount_has_unfinished_journals(store, mount_id)? {
        return Err(
            "A source push is still in progress. Wait for it to finish before resetting or remounting this source."
                .to_string(),
        );
    }

    if mount.projection.uses_virtual_filesystem() {
        reconcile_desktop_projection_changes(store, state_root, Some(&mount.root))?;
    }

    let preserved = if mount_has_pending_local_changes(store, state_root, mount_id)? {
        preserve_mount_pending_local_changes(store, state_root, mount_id)?
    } else {
        None
    };
    clear_mount_cached_projection(store, state_root, mount_id)?;

    Ok(preserved)
}

fn mount_has_pending_local_changes(
    store: &SqliteStateStore,
    _state_root: &Path,
    mount_id: &MountId,
) -> Result<bool, String> {
    if !store
        .list_virtual_mutations(mount_id)
        .map_err(|error| format!("Could not inspect pending virtual changes: {error}"))?
        .is_empty()
    {
        return Ok(true);
    }

    if store
        .list_entities(mount_id)
        .map_err(|error| format!("Could not inspect cached source items: {error}"))?
        .iter()
        .any(|entity| {
            matches!(
                entity.hydration,
                HydrationState::Dirty | HydrationState::Conflicted
            )
        })
    {
        return Ok(true);
    }

    Ok(false)
}

fn mount_has_unfinished_journals(
    store: &SqliteStateStore,
    mount_id: &MountId,
) -> Result<bool, String> {
    Ok(store
        .list_journal()
        .map_err(|error| format!("Could not inspect push journals: {error}"))?
        .iter()
        .any(|journal| {
            journal.mount_id == *mount_id
                && matches!(
                    journal.status,
                    JournalStatus::Prepared | JournalStatus::Applying | JournalStatus::Applied
                )
        }))
}

#[derive(Debug, PartialEq, Eq)]
struct PreservedLocalChanges {
    directory: PathBuf,
    count: usize,
}

#[derive(Serialize)]
struct PreservedLocalChangeManifest {
    mount_id: String,
    preserved_at: String,
    items: Vec<PreservedLocalChangeItem>,
}

#[derive(Serialize)]
struct PreservedLocalChangeItem {
    kind: String,
    title: String,
    path: String,
    remote_id: Option<String>,
    hydration: Option<String>,
    source_path: Option<String>,
    preserved_path: Option<String>,
}

fn preserve_mount_pending_local_changes(
    store: &SqliteStateStore,
    state_root: &Path,
    mount_id: &MountId,
) -> Result<Option<PreservedLocalChanges>, String> {
    let mount = store
        .get_mount(mount_id)
        .map_err(|error| format!("Could not inspect existing source mount: {error}"))?
        .ok_or_else(|| format!("Could not find existing source mount `{}`.", mount_id.0))?;
    let pending_entities = store
        .list_entities(mount_id)
        .map_err(|error| format!("Could not inspect cached source items: {error}"))?
        .into_iter()
        .filter(|entity| {
            matches!(
                entity.hydration,
                HydrationState::Dirty | HydrationState::Conflicted
            )
        })
        .collect::<Vec<_>>();
    let pending_mutations = store
        .list_virtual_mutations(mount_id)
        .map_err(|error| format!("Could not inspect pending virtual changes: {error}"))?;

    if pending_entities.is_empty() && pending_mutations.is_empty() {
        return Ok(None);
    }

    let preserved_at = activity_timestamp();
    let directory = state_root
        .join("recovered")
        .join(&mount_id.0)
        .join(preserved_at.replace(':', "-"));
    fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "Could not create local change recovery folder at `{}`: {error}",
            directory.display()
        )
    })?;

    let mut items = Vec::new();
    for entity in pending_entities {
        items.push(preserve_entity_local_change(
            state_root, &mount, &directory, entity,
        )?);
    }
    for mutation in pending_mutations {
        items.push(preserve_virtual_mutation_local_change(
            state_root, &mount, &directory, mutation,
        )?);
    }

    let manifest = PreservedLocalChangeManifest {
        mount_id: mount_id.0.clone(),
        preserved_at,
        items,
    };
    let manifest_path = directory.join("manifest.json");
    let manifest_json = serde_json::to_string_pretty(&manifest)
        .map_err(|error| format!("Could not serialize local change manifest: {error}"))?;
    fs::write(&manifest_path, manifest_json).map_err(|error| {
        format!(
            "Could not write local change manifest at `{}`: {error}",
            manifest_path.display()
        )
    })?;
    let readme_path = directory.join("README.md");
    fs::write(
        &readme_path,
        "Locality preserved these local Notion edits before refreshing the active mount for a changed Notion access scope.\n\nThe active mount was cleared so old pages outside the current Notion access do not keep appearing as pending changes. Review these files manually if you need to copy edits into the newly mounted workspace.\n",
    )
    .map_err(|error| {
        format!(
            "Could not write local change recovery README at `{}`: {error}",
            readme_path.display()
        )
    })?;

    Ok(Some(PreservedLocalChanges {
        directory,
        count: manifest.items.len(),
    }))
}

fn preserve_entity_local_change(
    state_root: &Path,
    mount: &MountConfig,
    recovery_dir: &Path,
    entity: EntityRecord,
) -> Result<PreservedLocalChangeItem, String> {
    let source_path = preserved_local_change_source_path(state_root, mount, &entity.path);
    let preserved_path = copy_preserved_file(source_path.as_deref(), recovery_dir, &entity.path)?;
    Ok(PreservedLocalChangeItem {
        kind: "entity".to_string(),
        title: entity.title,
        path: locality_platform::logical_path_display(&entity.path),
        remote_id: Some(entity.remote_id.0),
        hydration: Some(hydration_name(&entity.hydration).to_string()),
        source_path: source_path.map(|path| path.display().to_string()),
        preserved_path: preserved_path.map(|path| path.display().to_string()),
    })
}

fn preserve_virtual_mutation_local_change(
    state_root: &Path,
    mount: &MountConfig,
    recovery_dir: &Path,
    mutation: VirtualMutationRecord,
) -> Result<PreservedLocalChangeItem, String> {
    let fallback_path =
        preserved_local_change_source_path(state_root, mount, &mutation.projected_path);
    let source_path = mutation
        .content_path
        .clone()
        .filter(|path| path.exists())
        .or(fallback_path);
    let preserved_path = copy_preserved_file(
        source_path.as_deref(),
        recovery_dir,
        &mutation.projected_path,
    )?;
    Ok(PreservedLocalChangeItem {
        kind: format!("virtual_{:?}", mutation.mutation_kind).to_lowercase(),
        title: mutation.title,
        path: locality_platform::logical_path_display(&mutation.projected_path),
        remote_id: mutation.target_remote_id.map(|remote_id| remote_id.0),
        hydration: None,
        source_path: source_path.map(|path| path.display().to_string()),
        preserved_path: preserved_path.map(|path| path.display().to_string()),
    })
}

fn preserved_local_change_source_path(
    state_root: &Path,
    mount: &MountConfig,
    relative_path: &Path,
) -> Option<PathBuf> {
    if mount.projection.uses_virtual_filesystem() {
        virtual_fs_content_path(state_root, &mount.mount_id, relative_path).ok()
    } else {
        Some(mount.root.join(relative_path))
    }
}

fn copy_preserved_file(
    source_path: Option<&Path>,
    recovery_dir: &Path,
    relative_path: &Path,
) -> Result<Option<PathBuf>, String> {
    let Some(source_path) = source_path.filter(|path| path.is_file()) else {
        return Ok(None);
    };
    let destination = safe_recovery_path(recovery_dir, relative_path)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Could not create local change recovery folder at `{}`: {error}",
                parent.display()
            )
        })?;
    }
    fs::copy(source_path, &destination).map_err(|error| {
        format!(
            "Could not preserve local change from `{}` to `{}`: {error}",
            source_path.display(),
            destination.display()
        )
    })?;
    Ok(Some(destination))
}

fn safe_recovery_path(recovery_dir: &Path, relative_path: &Path) -> Result<PathBuf, String> {
    let mut destination = recovery_dir.to_path_buf();
    for component in relative_path.components() {
        match component {
            std::path::Component::Normal(part) => destination.push(part),
            std::path::Component::CurDir => {}
            _ => {
                return Err(format!(
                    "Could not preserve local change with unsafe path `{}`",
                    relative_path.display()
                ));
            }
        }
    }
    Ok(destination)
}

fn hydration_name(state: &HydrationState) -> &'static str {
    match state {
        HydrationState::Stub => "stub",
        HydrationState::Virtual => "virtual",
        HydrationState::Hydrated => "hydrated",
        HydrationState::Dirty => "dirty",
        HydrationState::Conflicted => "conflicted",
    }
}

fn clear_mount_cached_projection(
    store: &mut SqliteStateStore,
    state_root: &Path,
    mount_id: &MountId,
) -> Result<(), String> {
    let mount = store
        .get_mount(mount_id)
        .map_err(|error| format!("Could not inspect cached source mount: {error}"))?;
    let cached_entities = if mount.is_some() {
        store
            .list_entities(mount_id)
            .map_err(|error| format!("Could not inspect cached source items: {error}"))?
    } else {
        Vec::new()
    };

    if let Some(mount) = mount.as_ref() {
        clear_visible_projection_paths(mount, &cached_entities)?;
    }

    store
        .clear_mount_source_state(mount_id)
        .map_err(|error| format!("Could not clear cached source mount state: {error}"))?;

    let content_root = virtual_fs_content_root(state_root, mount_id);
    if content_root.exists() {
        fs::remove_dir_all(&content_root).map_err(|error| {
            format!(
                "Could not clear cached source file contents at `{}`: {error}",
                content_root.display()
            )
        })?;
    }

    Ok(())
}

fn clear_visible_projection_paths(
    mount: &MountConfig,
    entities: &[EntityRecord],
) -> Result<(), String> {
    if mount.projection == ProjectionMode::MacosFileProvider {
        return Ok(());
    }

    let mut removals = BTreeSet::new();
    for root in daemon_file_provider::mount_access_roots(mount) {
        for entity in entities {
            for relative_path in visible_projection_cleanup_paths(entity) {
                let path = safe_visible_projection_path(&root, &relative_path)?;
                removals.insert((root.clone(), path));
            }
        }
    }

    let mut removals = removals.into_iter().collect::<Vec<_>>();
    removals.sort_by(|(_, left), (_, right)| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| right.cmp(left))
    });

    for (root, path) in removals {
        remove_visible_projection_path(&root, &path)?;
    }

    Ok(())
}

fn visible_projection_cleanup_paths(entity: &EntityRecord) -> Vec<PathBuf> {
    match entity.kind {
        EntityKind::Database => vec![entity.path.join("_schema.yaml"), entity.path.clone()],
        EntityKind::Directory => vec![entity.path.clone()],
        EntityKind::Page | EntityKind::Asset | EntityKind::Unknown(_) => vec![entity.path.clone()],
    }
}

fn safe_visible_projection_path(root: &Path, relative_path: &Path) -> Result<PathBuf, String> {
    if relative_path.components().any(|component| {
        matches!(
            component,
            std::path::Component::Prefix(_)
                | std::path::Component::RootDir
                | std::path::Component::ParentDir
        )
    }) {
        return Err(format!(
            "Could not clear visible projection path with unsafe cached path `{}`.",
            relative_path.display()
        ));
    }
    Ok(root.join(relative_path))
}

fn remove_visible_projection_path(root: &Path, path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "Could not inspect visible stale Notion file `{}`: {error}",
                path.display()
            ));
        }
    };

    if metadata.is_dir() {
        match fs::remove_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => return Ok(()),
            Err(error) => {
                return Err(format!(
                    "Could not remove visible stale Notion folder `{}`: {error}",
                    path.display()
                ));
            }
        }
        prune_empty_visible_projection_parents(root, path.parent())?;
    } else {
        fs::remove_file(path).map_err(|error| {
            format!(
                "Could not remove visible stale Notion file `{}`: {error}",
                path.display()
            )
        })?;
        prune_empty_visible_projection_parents(root, path.parent())?;
    }

    Ok(())
}

fn prune_empty_visible_projection_parents(
    root: &Path,
    mut current: Option<&Path>,
) -> Result<(), String> {
    while let Some(directory) = current {
        if directory == root || !directory.starts_with(root) {
            break;
        }
        match fs::remove_dir(directory) {
            Ok(()) => current = directory.parent(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                current = directory.parent();
            }
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => break,
            Err(error) => {
                return Err(format!(
                    "Could not remove empty stale Notion folder `{}`: {error}",
                    directory.display()
                ));
            }
        }
    }
    Ok(())
}

fn wait_for_mount_entities(state_root: &Path, mount_id: &MountId) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let store = SqliteStateStore::open(state_root.to_path_buf())
            .map_err(|error| format!("Could not inspect refreshed Notion mount: {error}"))?;
        let count = store
            .list_entities(mount_id)
            .map_err(|error| format!("Could not inspect refreshed Notion mount: {error}"))?
            .len();
        if count > 0 || std::time::Instant::now() >= deadline {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn reusable_notion_connection_id(store: &SqliteStateStore) -> Option<ConnectionId> {
    store
        .list_connections()
        .ok()?
        .into_iter()
        .find(|connection| connection.connector == "notion")
        .map(|connection| connection.connection_id)
}

fn push_target_direct(target: &Path, confirm_dangerous: bool) -> Result<PushReport, String> {
    let state_root = default_state_root();
    push_target_direct_at_state_root(&state_root, target, confirm_dangerous)
}

fn push_target_direct_at_state_root(
    state_root: &Path,
    target: &Path,
    confirm_dangerous: bool,
) -> Result<PushReport, String> {
    let mut store = SqliteStateStore::open(state_root.to_path_buf())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let target = absolute_path(target)?;
    reconcile_desktop_projection_changes(&mut store, &state_root, Some(&target))?;
    let (mount, relative_path) = resolve_desktop_mount_path(&store, &target)?;
    let credentials = open_credential_store(&state_root);
    let connector = resolve_source_for_path(&store, credentials.as_ref(), &target)
        .map_err(|error| error.message())?;

    let report = run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        &target,
        PushOptions {
            assume_yes: true,
            confirm_dangerous,
        },
        Some(&state_root),
    )
    .map_err(|error| error.to_string())?;
    if push_report_exit_code(&report) == 0 {
        refresh_visible_target_from_cache(&state_root, &mount, &relative_path, &target)?;
    }
    Ok(report)
}

fn set_live_mode_for_file_blocking(change: LiveModeFileChange) -> Result<ActionReport, String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let target = expand_tilde(&change.path).unwrap_or_else(|_| PathBuf::from(&change.path));
    let target = absolute_path(&target)?;
    let (mount, relative_path) = resolve_desktop_mount_path(&store, &target)?;
    let existing = store
        .get_auto_save_enrollment(&mount.mount_id, &relative_path)
        .map_err(|error| error.to_string())?;
    let remote_id = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(|error| error.to_string())?
        .map(|entity| entity.remote_id);
    let now = auto_save_timestamp();

    let mut enrollment = existing.unwrap_or_else(|| {
        AutoSaveEnrollmentRecord::new(
            mount.mount_id.clone(),
            relative_path.clone(),
            auto_save_origin_for_path(&store, &mount.mount_id, &relative_path),
            now.clone(),
        )
    });
    enrollment.remote_id = remote_id;
    enrollment.enabled = change.enabled;
    enrollment.state = AutoSaveState::Active;
    enrollment.last_reason = None;
    enrollment.updated_at = now;
    store
        .save_auto_save_enrollment(enrollment)
        .map_err(|error| error.to_string())?;

    if change.enabled {
        let _ = auto_save_target_direct(&target);
    }

    Ok(ActionReport {
        ok: true,
        message: if change.enabled {
            "Live Mode is on for this file.".to_string()
        } else {
            "Live Mode is off for this file.".to_string()
        },
    })
}

fn auto_save_origin_for_path(
    store: &SqliteStateStore,
    mount_id: &MountId,
    relative_path: &Path,
) -> AutoSaveOrigin {
    let is_locality_created = store
        .find_virtual_mutation_by_path(mount_id, relative_path)
        .ok()
        .flatten()
        .is_some_and(|mutation| mutation.mutation_kind == VirtualMutationKind::Create)
        || store
            .find_entity_by_path(mount_id, relative_path)
            .ok()
            .flatten()
            .is_none();

    if is_locality_created {
        AutoSaveOrigin::LocalityCreated
    } else {
        AutoSaveOrigin::UserEnabled
    }
}

fn resolve_desktop_mount_path(
    store: &SqliteStateStore,
    target: &Path,
) -> Result<(MountConfig, PathBuf), String> {
    let mounts = store.load_mounts().map_err(|error| error.to_string())?;
    daemon_file_provider::find_mount_for_path(&mounts, target)
        .map(|(mount, matched)| (mount.clone(), matched.relative_path))
        .ok_or_else(|| format!("Path is not inside an Locality mount: {}", target.display()))
}

fn reconcile_desktop_projection_changes(
    store: &mut SqliteStateStore,
    state_root: &Path,
    target: Option<&Path>,
) -> Result<(), String> {
    daemon_file_provider::reconcile_visible_projection(store, state_root, target)
        .map(|_| ())
        .map_err(|error| format!("Could not reconcile visible projection changes: {error}"))
}

fn auto_save_target_direct(target: &Path) -> Result<PushReport, String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let target = absolute_path(target)?;
    reconcile_desktop_projection_changes(&mut store, &state_root, Some(&target))?;
    let (mount, relative_path) = resolve_desktop_mount_path(&store, &target)?;
    let credentials = open_credential_store(&state_root);
    let connector = resolve_source_for_path(&store, credentials.as_ref(), &target)
        .map_err(|error| error.message())?;
    let report = execute_auto_save_push_job_with_content_root(
        &mut store,
        localityd::execution::PushJob {
            target_path: target.clone(),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &connector,
        Some(&state_root),
    )
    .map_err(|error| error.to_string())?;
    let report = PushReport::from_daemon(report);
    if push_report_exit_code(&report) == 0 {
        refresh_visible_target_from_cache(&state_root, &mount, &relative_path, &target)?;
    }
    Ok(report)
}

fn pull_target_direct(target: &Path) -> Result<PullReport, String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let credentials = open_credential_store(&state_root);
    let connector = resolve_source_for_path(&store, credentials.as_ref(), target)
        .map_err(|error| error.message())?;

    run_pull_with_state_root(&mut store, &connector, target, Some(&state_root))
        .map_err(|error| error.message())
}

fn queue_observe_target_direct(target: &Path) -> Result<(), String> {
    let state_root = default_state_root();
    let store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let target = absolute_path(target)?;
    let (mount, relative_path) = resolve_desktop_mount_path(&store, &target)?;
    let entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(|error| format!("Could not inspect local metadata: {error}"))?
        .ok_or_else(|| {
            format!(
                "No Locality file metadata found for `{}`.",
                target.display()
            )
        })?;
    let request = DaemonRequest::ObserveEntity {
        mount_id: mount.mount_id.0,
        remote_id: entity.remote_id.0,
    };
    match send_request(&state_root, &request) {
        Ok(response) if response.ok => Ok(()),
        Ok(response) => {
            let message = response
                .error
                .map(|error| error.message)
                .unwrap_or_else(|| "daemon rejected the freshness check".to_string());
            Err(format!("Could not check Notion for this page: {message}"))
        }
        Err(error) => Err(format!(
            "Could not reach the Locality daemon to check Notion for this page: {}",
            error.message()
        )),
    }
}

fn keep_target_as_local_draft_direct(target: &Path) -> Result<PathBuf, String> {
    let state_root = default_state_root();
    let target = absolute_path(target)?;
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let (mount, relative_path) = resolve_desktop_mount_path(&store, &target)?;
    let entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(|error| format!("Could not inspect local metadata: {error}"))?
        .ok_or_else(|| {
            format!(
                "No Locality file metadata found for `{}`.",
                target.display()
            )
        })?;
    let source_path = local_draft_source_path(&state_root, &mount, &relative_path, &target)
        .ok_or_else(|| {
            format!(
                "No local draft contents are available for `{}`.",
                target.display()
            )
        })?;
    let recovery_dir = state_root
        .join("recovered")
        .join(&mount.mount_id.0)
        .join(activity_timestamp().replace(':', "-"));
    fs::create_dir_all(&recovery_dir).map_err(|error| {
        format!(
            "Could not create local draft recovery folder at `{}`: {error}",
            recovery_dir.display()
        )
    })?;
    let draft_path = copy_preserved_file(Some(&source_path), &recovery_dir, &relative_path)?
        .ok_or_else(|| {
            format!(
                "Could not preserve local draft contents from `{}`.",
                source_path.display()
            )
        })?;
    fs::write(
        recovery_dir.join("README.md"),
        "Locality preserved this file as a local draft after its Notion page was deleted or became unavailable.\n\nReview the draft manually if you want to copy its contents into a new Notion page.\n",
    )
    .map_err(|error| {
        format!(
            "Could not write local draft recovery README in `{}`: {error}",
            recovery_dir.display()
        )
    })?;

    store
        .delete_entity(&mount.mount_id, &entity.remote_id)
        .map_err(|error| format!("Could not remove deleted Notion page metadata: {error}"))?;
    store
        .delete_freshness_state(&mount.mount_id, &entity.remote_id)
        .map_err(|error| format!("Could not clear Notion freshness metadata: {error}"))?;
    store
        .delete_remote_observation(&mount.mount_id, &entity.remote_id)
        .map_err(|error| format!("Could not clear Notion deletion metadata: {error}"))?;
    if target.exists() {
        let _ = fs::remove_file(&target);
    }

    Ok(draft_path)
}

fn local_draft_source_path(
    state_root: &Path,
    mount: &MountConfig,
    relative_path: &Path,
    target: &Path,
) -> Option<PathBuf> {
    if mount.projection.uses_virtual_filesystem() {
        return virtual_fs_content_path(state_root, &mount.mount_id, relative_path)
            .ok()
            .filter(|path| path.is_file());
    }
    target.is_file().then(|| target.to_path_buf())
}

fn reset_target_to_remote_direct(target: &Path) -> Result<PullReport, String> {
    let state_root = default_state_root();
    let target = absolute_path(target)?;
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let (mount, relative_path) = resolve_desktop_mount_path(&store, &target)?;

    run_restore(
        &mut store,
        &target,
        RestoreOptions {
            force: true,
            state_root: Some(state_root.clone()),
        },
    )
    .map_err(|error| error.message())?;
    refresh_visible_target_from_cache(&state_root, &mount, &relative_path, &target)?;

    let credentials = open_credential_store(&state_root);
    let connector = resolve_source_for_path(&store, credentials.as_ref(), &target)
        .map_err(|error| error.message())?;

    run_pull_with_state_root(&mut store, &connector, target, Some(&state_root))
        .map_err(|error| error.message())
}

fn refresh_visible_target_from_cache(
    state_root: &Path,
    mount: &MountConfig,
    relative_path: &Path,
    target: &Path,
) -> Result<(), String> {
    if !mount.projection.uses_virtual_filesystem() || !target.is_file() {
        return Ok(());
    }
    let content_path = virtual_fs_content_path(state_root, &mount.mount_id, relative_path)
        .map_err(|error| error.to_string())?;
    let contents =
        fs::read(&content_path).map_err(|error| format!("Could not read local cache: {error}"))?;
    write_file_atomic(target, &contents)
        .map_err(|error| format!("Could not refresh visible file from local cache: {error}"))
}

fn diff_target_direct(target: &Path) -> Result<DiffReport, String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    reconcile_desktop_projection_changes(&mut store, &state_root, Some(target))?;

    run_diff(&store, target).map_err(|error| error.message())
}

fn inspect_notion_file_blocking(target: &Path) -> FileDetailReport {
    match read_projected_file_contents(target) {
        Ok(contents) => {
            let conflict_preview = conflict_preview(&contents);
            let has_conflict_markers = conflict_preview.is_some();
            FileDetailReport {
                ok: true,
                path: target.display().to_string(),
                has_conflict_markers,
                conflict_preview,
                message: if has_conflict_markers {
                    "This file contains unresolved conflict markers. Choose the final Markdown content, remove the markers, then push again.".to_string()
                } else {
                    "No inline conflict markers were found in the local file.".to_string()
                },
            }
        }
        Err(message) => FileDetailReport {
            ok: false,
            path: target.display().to_string(),
            has_conflict_markers: false,
            conflict_preview: None,
            message,
        },
    }
}

fn read_notion_file_blocking(target: &Path) -> FileEditorReport {
    match read_projected_file_contents(target) {
        Ok(contents) => FileEditorReport {
            ok: true,
            path: target.display().to_string(),
            has_conflict_markers: has_unresolved_conflict_markers(&contents),
            message: "Loaded local Markdown.".to_string(),
            contents,
        },
        Err(message) => FileEditorReport {
            ok: false,
            path: target.display().to_string(),
            contents: String::new(),
            has_conflict_markers: false,
            message,
        },
    }
}

fn save_notion_file_blocking(target: &Path, contents: &str) -> Result<String, String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Could not inspect Locality mounts: {error}"))?;
    let Some((mount, matched)) = daemon_file_provider::find_mount_for_path(&mounts, target) else {
        return Err(format!(
            "No Locality mount contains `{}`.",
            target.display()
        ));
    };
    let mount = mount.clone();
    let relative_path = matched.relative_path;
    let entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(|error| format!("Could not find mounted file metadata: {error}"))?
        .ok_or_else(|| {
            format!(
                "No Locality file metadata found for `{}`.",
                target.display()
            )
        })?;

    let hydration = if mount.projection.uses_virtual_filesystem() {
        let report = commit_virtual_fs_write(
            &mut store,
            &virtual_fs_content_root(&state_root, &mount.mount_id),
            &mount.mount_id,
            &entity.remote_id.0,
            contents.as_bytes(),
        )
        .map_err(|error| error.to_string())?;
        report.hydration
    } else {
        let write_path = mount.root.join(&relative_path);
        write_file_atomic(&write_path, contents.as_bytes())
            .map_err(|error| format!("Could not write `{}`: {error}", write_path.display()))?;
        update_entity_after_editor_write(&mut store, entity, contents)?
    };

    Ok(match hydration {
        HydrationState::Conflicted => {
            "Saved local Markdown with unresolved conflict markers.".to_string()
        }
        HydrationState::Hydrated => {
            "Saved local Markdown; it matches the synced version.".to_string()
        }
        _ => "Saved local Markdown as pending changes.".to_string(),
    })
}

fn update_entity_after_editor_write(
    store: &mut SqliteStateStore,
    mut entity: locality_store::EntityRecord,
    contents: &str,
) -> Result<HydrationState, String> {
    let next = hydration_after_editor_write(store, &entity, contents);
    entity.hydration = next.clone();
    store
        .save_entity(entity)
        .map_err(|error| format!("Could not update local file state: {error}"))?;
    Ok(next)
}

fn hydration_after_editor_write(
    store: &SqliteStateStore,
    entity: &locality_store::EntityRecord,
    contents: &str,
) -> HydrationState {
    if has_unresolved_conflict_markers(contents) {
        return HydrationState::Conflicted;
    }
    if editor_contents_match_shadow(store, entity, contents) {
        return HydrationState::Hydrated;
    }
    HydrationState::Dirty
}

fn editor_contents_match_shadow(
    store: &SqliteStateStore,
    entity: &locality_store::EntityRecord,
    contents: &str,
) -> bool {
    parse_canonical_markdown(contents)
        .ok()
        .and_then(|parsed| {
            store
                .load_shadow(&entity.mount_id, &entity.remote_id)
                .ok()
                .map(|shadow| {
                    parsed.document.frontmatter == shadow.frontmatter
                        && parsed.document.body == shadow.rendered_body
                })
        })
        .unwrap_or(false)
}

fn read_projected_file_contents(target: &Path) -> Result<String, String> {
    let state_root = default_state_root();
    let store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Could not inspect Locality mounts: {error}"))?;
    let Some((mount, matched)) = daemon_file_provider::find_mount_for_path(&mounts, target) else {
        return Err(format!(
            "No Locality mount contains `{}`.",
            target.display()
        ));
    };
    let read_path = if mount.projection.uses_virtual_filesystem() {
        virtual_fs_content_path(&state_root, &mount.mount_id, &matched.relative_path)
            .map_err(|error| error.to_string())?
    } else {
        target.to_path_buf()
    };

    fs::read_to_string(&read_path)
        .map_err(|error| format!("Could not read `{}`: {error}", read_path.display()))
}

fn write_file_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp-locality-desktop");
    if let Err(error) = fs::write(&tmp, contents) {
        if path.exists() {
            return fs::write(path, contents).map_err(|_| error);
        }
        return Err(error);
    }
    fs::rename(&tmp, path).or_else(|error| {
        let _ = fs::remove_file(&tmp);
        if path.exists() {
            fs::write(path, contents).map_err(|_| error)
        } else {
            Err(error)
        }
    })
}

fn push_report_message(report: &PushReport) -> String {
    push_action_message(report.action.as_str(), report.ok, report.message.as_deref())
}

fn push_action_message(action: &str, ok: bool, message: Option<&str>) -> String {
    match message {
        Some(message) if is_notion_access_lost_message(message) => {
            notion_access_lost_recovery_message()
        }
        Some(message) if is_remote_changed_push_message(message) => {
            "Notion has newer changes than your last sync. Pull latest on this file, resolve any conflict markers if Locality writes them, then push again.".to_string()
        }
        Some(message) if !message.is_empty() => message.to_string(),
        _ if ok => "Pushed changes to Notion.".to_string(),
        _ if action == "confirm_dangerous_plan" => {
            "This push needs review because it may move, archive, or touch a large amount of Notion content. Open Review Push to approve it.".to_string()
        }
        _ if action == "confirm_plan" => {
            "This push needs review before it writes to Notion. Open Review Push to approve it.".to_string()
        }
        _ if action == "read_only_blocked" => {
            "This mount is read-only, so Locality cannot push local edits to Notion.".to_string()
        }
        _ => format!("Push stopped: {action}"),
    }
}

fn pull_error_message(message: &str) -> String {
    if is_notion_access_lost_message(message) {
        return notion_access_lost_recovery_message();
    }
    message.to_string()
}

fn pull_report_message(report: &PullReport) -> String {
    if !report.conflicts.is_empty() {
        return "Pulled the latest Notion version and wrote conflict markers into the local file. Open the file, resolve the markers, then push again.".to_string();
    }
    if report.skipped_dirty > 0
        && (report.hydrated > 0 || report.enumerated > 0 || report.stubbed > 0)
    {
        return "Synced available Notion updates and kept pending local edits unchanged. Use Push or Reset to remote for the pending files.".to_string();
    }
    if report.hydrated > 0 {
        return "Synced the latest Notion version for this file.".to_string();
    }
    if report.skipped_dirty > 0 {
        return "Locality kept your local edits because the file is still dirty. Review the diff, then push or reset the file to remote.".to_string();
    }
    if report.enumerated > 0 || report.stubbed > 0 {
        return "Synced the latest Notion index for this mount.".to_string();
    }
    "Pulled the latest Notion content.".to_string()
}

fn reset_to_remote_message(report: &PullReport) -> String {
    if !report.conflicts.is_empty() || report.skipped_dirty > 0 {
        return pull_report_message(report);
    }
    if report.hydrated > 0 {
        return "Discarded local edits and restored the latest Notion version for this file."
            .to_string();
    }
    if report.enumerated > 0 || report.stubbed > 0 {
        return "Discarded local edits and refreshed the Notion index for this mount.".to_string();
    }
    "Discarded local edits and restored the synced Notion version for this file.".to_string()
}

fn diff_report_message(report: &DiffReport) -> String {
    if let Some(message) = &report.message
        && !message.is_empty()
    {
        return message.clone();
    }
    if let Some(issue) = report.validation.first() {
        return match issue.line {
            Some(line) => format!("{}:{}: {}", issue.file, line, issue.message),
            None => format!("{}: {}", issue.file, issue.message),
        };
    }

    let Some(plan) = &report.plan else {
        return "No local changes in this file.".to_string();
    };

    let mut parts = Vec::new();
    if plan.summary.blocks_updated > 0 {
        parts.push(format!("{} updated", plan.summary.blocks_updated));
    }
    if plan.summary.blocks_replaced > 0 {
        parts.push(format!("{} replaced", plan.summary.blocks_replaced));
    }
    if plan.summary.blocks_created > 0 {
        parts.push(format!("{} created", plan.summary.blocks_created));
    }
    if plan.summary.blocks_moved > 0 {
        parts.push(format!("{} moved", plan.summary.blocks_moved));
    }
    if plan.summary.blocks_archived > 0 {
        parts.push(format!("{} archived", plan.summary.blocks_archived));
    }
    if plan.summary.properties_updated > 0 {
        parts.push(format!("{} properties", plan.summary.properties_updated));
    }
    if parts.is_empty() {
        parts.push(format!("{} operations", plan.operations.len()));
    }

    let mut message = format!("Diff: {}.", parts.join(", "));
    if report.guardrail.decision == "confirm_required" {
        message.push_str(" Review required before pushing.");
    }
    message
}

fn is_remote_changed_push_message(message: &str) -> bool {
    message.contains("changed since last sync") && message.contains("remote entity")
}

fn is_notion_access_lost_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("notion api returned http 404")
        && (lower.contains("object_not_found")
            || lower.contains("could not find page")
            || lower.contains("could not find block")
            || lower.contains("could not find database"))
}

fn notion_access_lost_recovery_message() -> String {
    "This local page belongs to a Notion page that is outside the current selected access. Use Change Notion Access to include that page or teamspace, or keep the local copy as a recovered draft before refreshing the mount.".to_string()
}

fn join_mount_path(mount_path: &str, relative_path: &str) -> String {
    if relative_path.starts_with('/') || relative_path.starts_with("~/") {
        return relative_path.to_string();
    }
    format!(
        "{}/{}",
        mount_path.trim_end_matches('/'),
        relative_path.trim_start_matches('/')
    )
}

fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| std::env::var(key).ok())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::LiveModeRemoteDriftMerge;

    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use loc_cli::search::{SearchRemoteState, SearchResult, SearchSafety};
    use locality_core::canonical::render_canonical_markdown;
    use locality_core::freshness::{FreshnessTier, RemoteVersion};
    use locality_core::journal::{JournalEntry, JournalStatus, PushId};
    use locality_core::model::{
        CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
    };
    use locality_core::planner::PushPlan;
    use locality_core::shadow::ShadowDocument;
    use locality_store::{
        AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, ConnectionId,
        ConnectionRecord, ConnectionRepository, ConnectorProfileId, EntityRecord, EntityRepository,
        FreshnessStateRecord, FreshnessStateRepository, InMemoryStateStore, JournalRepository,
        LIVE_MODE_STATE_CHANGE_SIGNAL_FILE, MountConfig, MountLiveModeRecord,
        MountLiveModeRepository, MountLiveModeState, MountRepository, ProjectionMode,
        RemoteObservationRecord, RemoteObservationRepository, ShadowRepository, SqliteStateStore,
        StoreResult,
    };
    use localityd::ipc::DaemonBuildInfo;
    use tauri::{PhysicalPosition, PhysicalSize, Rect};

    use super::{
        ActionReport, DESKTOP_ACTIVITY_LIMIT, DESKTOP_INSTALL_MARKER_VERSION,
        LIVE_MODE_REMOTE_FAST_FORWARD_LEASE, LIVE_MODE_RUNNER_ACTIVE_INTERVAL,
        LIVE_MODE_RUNNER_PERIODIC_RECHECK, MonitorScreenBounds, PendingChange, ScreenBounds,
        TerminalCliLinkState, TrayVisualState, acknowledge_install_state_at, activity_timestamp,
        clear_mount_cached_projection, clear_state_root_contents, clear_visible_projection_paths,
        conflict_preview, connection_metadata_changed, current_daemon_build_id,
        current_desktop_build_id, diff_report_message, exact_located_entity_record,
        exact_notion_entry_matches, failed_push_summary, has_unresolved_conflict_markers,
        hydration_after_editor_write, inspect_install_state, install_terminal_cli_link_at,
        install_terminal_cli_link_in_path_dirs, is_notion_access_lost_message,
        is_unsupported_schema_version_message, live_mode_claim_remote_fast_forward_key,
        live_mode_enabled_mount, live_mode_local_reconcile_targets_for_mount_at,
        live_mode_merge_remote_drift_markdown, live_mode_release_remote_fast_forward_key,
        live_mode_remote_check_page_budget_for_rate, live_mode_remote_pull_candidates,
        live_mode_remote_pull_scan_is_due_for_key, live_mode_runner_should_tick,
        live_mode_should_reconcile_local_target_for_key, live_mode_target,
        live_mode_tick_from_snapshot, live_mode_wake_generation, load_desktop_activity,
        macos_app_bundle_for_exe, macos_file_provider_child_item_count,
        macos_file_provider_mount_root_health_error,
        macos_file_provider_mount_root_inspection_recovery_reason,
        macos_file_provider_mount_root_is_missing, macos_file_provider_mount_root_recovery_reason,
        mark_mount_live_mode_syncing, mount_has_pending_local_changes,
        mount_has_unfinished_journals, notion_id_from_url, parse_daemon_build_info_json,
        pending_changes_from_status, prepare_existing_workspace_mount_for_remount,
        preserve_mount_pending_local_changes, pull_error_message, pull_report_message,
        push_action_message, record_current_install_marker, record_desktop_activity,
        record_mount_live_mode_tick_result, refresh_mount_root_after_access_change,
        refresh_visible_target_from_cache, reset_to_remote_message, sample_live_mode_status,
        sample_snapshot, screen_bounds_for_anchor_from_monitors, shell_single_quote,
        should_hide_tray_popover, should_prioritize_located_result,
        state_event_path_requires_refresh, state_event_path_wakes_live_mode,
        summarize_virtual_projection_children, terminal_cli_link_state, tray_icon_image,
        tray_icon_should_use_template, tray_popover_anchor, tray_popover_position,
        unsupported_notion_locator_url_message, validate_mount_root,
        validate_source_action_confirmation, virtual_projection_prefetch_container_identifiers,
        virtual_projection_refresh_signal_identifiers,
        virtual_projection_source_ready_timeout_message,
        virtual_projection_waits_for_mount_point_children_before_registration,
        wait_for_live_mode_state_change, wake_live_mode_runner, write_terminal_cli_path_section,
    };
    #[cfg(test)]
    use super::{
        LiveModeE2eCleanup, live_mode_e2e_append_local_marker, live_mode_e2e_append_remote_marker,
        live_mode_e2e_context, live_mode_e2e_remote_text, live_mode_e2e_wait_until,
        live_mode_tick_blocking_at_state_root, unique_suffix,
    };
    #[cfg(target_os = "windows")]
    use super::{
        WindowsCloudFilesRuntimeProcess, windows_cloud_files_runtime_processes_from_state,
    };

    #[test]
    fn desktop_state_root_absolutizes_relative_fallbacks() {
        assert!(super::absolute_state_root(PathBuf::from(".loc")).is_absolute());
    }

    #[cfg(unix)]
    #[test]
    fn write_file_atomic_falls_back_to_existing_file_overwrite() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TestTempDir::new("atomic-write-fallback");
        let path = temp.path().join("page.md");
        fs::write(&path, "old").expect("write existing file");
        let mut readonly = fs::metadata(temp.path()).expect("metadata").permissions();
        readonly.set_mode(0o555);
        fs::set_permissions(temp.path(), readonly).expect("make parent readonly");

        let result = super::write_file_atomic(&path, b"new");

        let mut writable = fs::metadata(temp.path()).expect("metadata").permissions();
        writable.set_mode(0o755);
        fs::set_permissions(temp.path(), writable).expect("restore parent permissions");
        result.expect("overwrite existing file");
        assert_eq!(fs::read_to_string(&path).expect("read file"), "new");
    }

    #[test]
    fn macos_app_bundle_for_exe_resolves_bundle_root() {
        let executable =
            PathBuf::from("/Applications/Locality.app/Contents/MacOS/locality-desktop");

        assert_eq!(
            macos_app_bundle_for_exe(&executable),
            Some(PathBuf::from("/Applications/Locality.app"))
        );
    }

    #[test]
    fn macos_app_bundle_for_exe_ignores_plain_executables() {
        let executable = PathBuf::from("/usr/local/bin/locality-desktop");

        assert_eq!(macos_app_bundle_for_exe(&executable), None);
    }

    #[test]
    fn notion_access_miss_message_names_selected_scope() {
        let message = super::notion_access_miss_message_from_parts(
            "Synergy Labs",
            "Product Teamspace",
            Some("https://www.notion.so/37b3ac0ebb88802cbcf4d53c9cfc4972"),
        );

        assert!(message.contains("workspace `Synergy Labs`"));
        assert!(message.contains("Current mount access: `Product Teamspace`"));
        assert!(message.contains("select this page, database, or the correct teamspace"));
        assert!(message.contains("https://www.notion.so/37b3ac0ebb88802cbcf4d53c9cfc4972"));
    }

    #[test]
    fn exact_located_entity_updates_clean_stub_path() {
        let existing = EntityRecord::new(
            MountId::new("notion-main"),
            RemoteId::new("page-1"),
            EntityKind::Page,
            "Daily Standup",
            "engineering-wiki/daily-standup/page.md",
        )
        .with_hydration(HydrationState::Stub);
        let entry = tree_entry(
            "page-1",
            "Daily Standup",
            "engineering-wiki/standups-with-locality/daily-standup/page.md",
        );

        let record =
            exact_located_entity_record(&entry, Some(&existing)).expect("clean stub can move");

        assert_eq!(record.path, entry.path);
        assert_eq!(record.hydration, HydrationState::Stub);
    }

    #[test]
    fn exact_located_entity_blocks_dirty_path_move() {
        let existing = EntityRecord::new(
            MountId::new("notion-main"),
            RemoteId::new("page-1"),
            EntityKind::Page,
            "Daily Standup",
            "engineering-wiki/daily-standup/page.md",
        )
        .with_hydration(HydrationState::Dirty);
        let entry = tree_entry(
            "page-1",
            "Daily Standup",
            "engineering-wiki/standups-with-locality/daily-standup/page.md",
        );

        let error = exact_located_entity_record(&entry, Some(&existing))
            .expect_err("dirty old path must not move silently");

        assert!(error.contains("pending changes"));
        assert!(error.contains("engineering-wiki/daily-standup/page.md"));
        assert!(error.contains("engineering-wiki/standups-with-locality/daily-standup/page.md"));
    }

    #[test]
    fn daemon_build_info_parser_reads_sidecar_metadata() {
        let parsed = parse_daemon_build_info_json(br#"{"version":"0.1.3","build_id":"build-123"}"#)
            .expect("parse build info");

        assert_eq!(
            parsed,
            DaemonBuildInfo {
                version: "0.1.3".to_string(),
                build_id: "build-123".to_string(),
            }
        );
    }

    #[test]
    fn daemon_process_started_before_bundled_binary_compares_modified_times() {
        let pid_modified = UNIX_EPOCH + Duration::from_secs(10);
        let binary_modified = UNIX_EPOCH + Duration::from_secs(20);

        assert!(super::daemon_process_started_before_bundled_binary(
            Some(pid_modified),
            Some(binary_modified)
        ));
        assert!(!super::daemon_process_started_before_bundled_binary(
            Some(binary_modified),
            Some(pid_modified)
        ));
        assert!(!super::daemon_process_started_before_bundled_binary(
            None,
            Some(binary_modified)
        ));
    }

    #[test]
    fn daemon_lifecycle_lock_serializes_concurrent_callers() {
        let guard = super::daemon_lifecycle_lock()
            .lock()
            .expect("daemon lifecycle lock");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();

        let thread = std::thread::spawn(move || {
            started_tx.send(()).expect("send started");
            let _guard = super::daemon_lifecycle_lock()
                .lock()
                .expect("daemon lifecycle lock from worker");
            entered_tx.send(()).expect("send entered");
        });

        started_rx.recv().expect("worker started");
        assert!(entered_rx.recv_timeout(Duration::from_millis(30)).is_err());
        drop(guard);
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker entered after lock release");
        thread.join().expect("worker joined");
    }

    #[test]
    fn mount_summary_default_path_is_absolute() {
        let summary = super::mount_summary(None, Path::new("."), None, None, None);

        assert!(Path::new(&summary.local_path).is_absolute());
    }

    #[test]
    fn desktop_snapshot_lists_all_mounts_and_marks_active_mount() {
        let temp = TestTempDir::new("desktop-all-mounts");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        let notion_connection = test_connection("workspace-1", "CodeFlash");
        let google_connection = ConnectionRecord {
            connection_id: ConnectionId::new("google-docs-default"),
            profile_id: Some(ConnectorProfileId("google-docs-oauth-default".to_string())),
            connector: "google-docs".to_string(),
            display_name: "google-docs-default".to_string(),
            account_label: Some("mohammed@example.com".to_string()),
            workspace_id: None,
            workspace_name: None,
            auth_kind: "oauth".to_string(),
            secret_ref: "connection:google-docs-default".to_string(),
            scopes: Vec::new(),
            capabilities_json: "{}".to_string(),
            status: "active".to_string(),
            created_at: "1".to_string(),
            updated_at: "1".to_string(),
            expires_at: None,
        };
        let notion_mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            temp.path().join("codeflash-wiki"),
        )
        .with_connection_id(notion_connection.connection_id.clone())
        .projection(ProjectionMode::LinuxFuse);
        let google_mount = MountConfig::new(
            MountId::new("google-docs-main"),
            "google-docs",
            temp.path().join("google-docs"),
        )
        .with_connection_id(google_connection.connection_id.clone())
        .with_remote_root_id(RemoteId::new("drive-folder-1"))
        .projection(ProjectionMode::LinuxFuse);

        store
            .save_connection(notion_connection)
            .expect("save notion connection");
        store
            .save_connection(google_connection)
            .expect("save google connection");
        store.save_mount(google_mount).expect("save google mount");
        store.save_mount(notion_mount).expect("save notion mount");
        let notion_remote_id = RemoteId::new("notion-page-1");
        store
            .save_entity(
                EntityRecord::new(
                    MountId::new("notion-main"),
                    notion_remote_id.clone(),
                    EntityKind::Page,
                    "Standups with Locality",
                    "Standups with Locality/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save notion entity");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    MountId::new("notion-main"),
                    notion_remote_id.clone(),
                    FreshnessTier::Hot,
                )
                .opened_at("unix_ms:1782033300000"),
            )
            .expect("save notion freshness");
        let google_remote_id = RemoteId::new("doc-1");
        store
            .save_entity(
                EntityRecord::new(
                    MountId::new("google-docs-main"),
                    google_remote_id.clone(),
                    EntityKind::Page,
                    "Planning Doc",
                    "planning-doc/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save google entity");
        store
            .append_journal(JournalEntry::new(
                PushId("push-google-1".to_string()),
                MountId::new("google-docs-main"),
                vec![google_remote_id.clone()],
                PushPlan::new(vec![google_remote_id], Vec::new()),
                JournalStatus::Prepared,
            ))
            .expect("append google journal");

        let snapshot = super::load_desktop_snapshot_from_store(&store, temp.path())
            .expect("load snapshot from test store");

        assert_eq!(snapshot.mounts.len(), 2);
        assert_eq!(snapshot.active_mount_id.as_deref(), Some("notion-main"));
        assert_eq!(snapshot.mount.mount_id, "notion-main");
        assert_eq!(
            snapshot
                .connections
                .iter()
                .map(|connection| connection.connector.as_str())
                .collect::<Vec<_>>(),
            vec!["notion", "google-docs"]
        );
        assert_eq!(
            snapshot
                .mounts
                .iter()
                .map(|mount| mount.mount_id.as_str())
                .collect::<Vec<_>>(),
            vec!["google-docs-main", "notion-main"]
        );
        assert_eq!(
            snapshot
                .mounts
                .iter()
                .find(|mount| mount.mount_id == "google-docs-main")
                .expect("google mount")
                .connector_name,
            "Google Docs"
        );
        assert_eq!(snapshot.recent_files.len(), 1);
        assert_eq!(snapshot.recent_files[0].title, "Standups with Locality");
        assert!(
            snapshot.recent_files[0]
                .local_path
                .replace('\\', "/")
                .ends_with("Standups with Locality/page.md")
        );
        assert_eq!(
            snapshot
                .mounts
                .iter()
                .find(|mount| mount.mount_id == "google-docs-main")
                .expect("google mount")
                .pending_change_count,
            1
        );
    }

    #[test]
    fn desktop_snapshot_hides_recent_files_for_inactive_mount_access() {
        let temp = TestTempDir::new("desktop-stale-recent-files");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        let mut connection = test_connection("workspace-1", "CodeFlash");
        connection.status = "revoked".to_string();
        let remote_id = RemoteId::new("stale-page-1");
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            temp.path().join("codeflash-wiki"),
        )
        .with_connection_id(connection.connection_id.clone())
        .projection(ProjectionMode::LinuxFuse);
        store.save_connection(connection).expect("save connection");
        store.save_mount(mount).expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    MountId::new("notion-main"),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Old Access Page",
                    "Old Access Page/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save stale entity");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    MountId::new("notion-main"),
                    remote_id,
                    FreshnessTier::Hot,
                )
                .opened_at("unix_ms:1782033300000"),
            )
            .expect("save stale freshness");

        let snapshot = super::load_desktop_snapshot_from_store(&store, temp.path())
            .expect("load snapshot from test store");

        assert!(snapshot.needs_onboarding);
        assert!(snapshot.recent_files.is_empty());
    }

    #[test]
    fn desktop_snapshot_prefers_active_granola_over_stale_notion_mount() {
        let temp = TestTempDir::new("desktop-granola-first");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        let mut stale_notion = test_connection("workspace-1", "Old Notion");
        stale_notion.status = "revoked".to_string();
        let granola_connection = ConnectionRecord {
            connection_id: ConnectionId::new("granola-default"),
            profile_id: None,
            connector: "granola".to_string(),
            display_name: "granola-default".to_string(),
            account_label: Some("Granola".to_string()),
            workspace_id: None,
            workspace_name: Some("Granola".to_string()),
            auth_kind: "api_key".to_string(),
            secret_ref: "connection:granola-default".to_string(),
            scopes: Vec::new(),
            capabilities_json: "{}".to_string(),
            status: "active".to_string(),
            created_at: "1".to_string(),
            updated_at: "1".to_string(),
            expires_at: None,
        };
        let notion_mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            temp.path().join("notion"),
        )
        .with_connection_id(stale_notion.connection_id.clone())
        .projection(ProjectionMode::LinuxFuse);
        let granola_mount = MountConfig::new(
            MountId::new("granola-main"),
            "granola",
            temp.path().join("granola"),
        )
        .with_connection_id(granola_connection.connection_id.clone())
        .read_only(true)
        .projection(ProjectionMode::LinuxFuse);

        store
            .save_connection(stale_notion)
            .expect("save stale notion connection");
        store
            .save_connection(granola_connection)
            .expect("save granola connection");
        store
            .save_mount(notion_mount)
            .expect("save stale notion mount");
        store.save_mount(granola_mount).expect("save granola mount");

        let snapshot = super::load_desktop_snapshot_from_store(&store, temp.path())
            .expect("load snapshot from test store");

        assert_eq!(snapshot.active_mount_id.as_deref(), Some("granola-main"));
        assert_eq!(snapshot.mount.connector, "granola");
        assert_eq!(snapshot.connection.connector, "granola");
        assert_eq!(
            snapshot
                .connections
                .iter()
                .map(|connection| connection.connector.as_str())
                .collect::<Vec<_>>(),
            vec!["notion", "granola"]
        );
        assert!(!snapshot.needs_onboarding);
    }

    #[test]
    fn desktop_mount_by_id_returns_selected_mount_not_preferred_mount() {
        let temp = TestTempDir::new("desktop-selected-mount-by-id");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        let notion = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            temp.path().join("notion"),
        );
        let google = MountConfig::new(
            MountId::new("google-docs-main"),
            "google-docs",
            temp.path().join("google-docs"),
        );
        store.save_mount(notion).expect("save notion mount");
        store.save_mount(google.clone()).expect("save google mount");

        let selected =
            super::desktop_mount_by_id(&store, "google-docs-main").expect("selected mount");

        assert_eq!(selected.mount_id, google.mount_id);
        assert_eq!(selected.connector, "google-docs");
    }

    #[test]
    fn desktop_onboarding_is_required_until_active_connection_and_mount_exist() {
        let active_connection = test_connection("workspace-1", "Synergy Labs");
        let mut inactive_connection = active_connection.clone();
        inactive_connection.status = "revoked".to_string();
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion",
        );

        assert!(super::desktop_needs_onboarding(None, None));
        assert!(super::desktop_needs_onboarding(
            Some(&inactive_connection),
            Some(&mount),
        ));
        assert!(super::desktop_needs_onboarding(
            Some(&active_connection),
            None,
        ));
        assert!(!super::desktop_needs_onboarding(
            Some(&active_connection),
            Some(&mount),
        ));
    }

    #[test]
    fn live_mode_tick_noops_without_pending_changes() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes.clear();
        let mut sync_calls = 0usize;
        let mut fast_forward_calls = 0usize;

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            &[],
            |_, _| {
                sync_calls += 1;
                Ok(())
            },
            |_| {
                fast_forward_calls += 1;
                Ok(())
            },
            |_, _| panic!("no pending changes should not merge remote drift"),
        );

        assert!(report.ok);
        assert_eq!(report.message, "Live Mode checked for changes.");
        assert_eq!(sync_calls, 0);
        assert_eq!(fast_forward_calls, 0);
    }

    #[test]
    fn live_mode_tick_queues_remote_candidate_without_pending_changes() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes.clear();
        let target = live_mode_target("/tmp/Locality/notion/teamspace-home/hello-world/page.md");
        let mut sync_calls = 0usize;
        let mut queued = Vec::new();

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            std::slice::from_ref(&target),
            |_, _| {
                sync_calls += 1;
                Ok(())
            },
            |target| {
                queued.push(target.path.clone());
                Ok(())
            },
            |_, _| panic!("remote-only tick should not merge local drift"),
        );

        assert!(report.ok);
        assert_eq!(report.message, "Live Mode queued 1 remote update.");
        assert_eq!(sync_calls, 0);
        assert_eq!(queued, vec![target.path]);
    }

    #[test]
    fn live_mode_tick_queues_bounded_remote_candidate_batch_without_pending_changes() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes.clear();
        let first = live_mode_target("/tmp/Locality/notion/teamspace-home/hello-world/page.md");
        let second = live_mode_target("/tmp/Locality/notion/teamspace-home/roadmap/page.md");
        let targets = vec![first.clone(), second.clone()];
        let mut queued = Vec::new();

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            &targets,
            |_, _| panic!("remote-only tick should not sync local changes"),
            |target| {
                queued.push(target.path.clone());
                Ok(())
            },
            |_, _| panic!("remote-only tick should not merge local drift"),
        );

        assert!(report.ok);
        assert_eq!(report.message, "Live Mode queued 2 remote updates.");
        assert_eq!(queued, vec![first.path, second.path]);
    }

    #[test]
    fn live_mode_remote_check_page_budget_tracks_notion_rps() {
        assert_eq!(
            live_mode_remote_check_page_budget_for_rate(3.0, Duration::from_secs(5)),
            5
        );
        assert_eq!(
            live_mode_remote_check_page_budget_for_rate(0.25, Duration::from_secs(3)),
            1
        );
        assert_eq!(
            live_mode_remote_check_page_budget_for_rate(f64::NAN, Duration::from_secs(5)),
            1
        );
    }

    #[test]
    fn live_mode_local_reconcile_targets_recent_active_pages() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion",
        )
        .projection(ProjectionMode::MacosFileProvider);
        let mut store = InMemoryStateStore::new();
        store.save_mount(mount.clone()).expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount.mount_id.clone(),
                    RemoteId::new("recent-page"),
                    EntityKind::Page,
                    "Recent",
                    "Recent/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save recent entity");
        store
            .save_entity(
                EntityRecord::new(
                    mount.mount_id.clone(),
                    RemoteId::new("cold-page"),
                    EntityKind::Page,
                    "Cold",
                    "Cold/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save cold entity");
        store
            .save_entity(
                EntityRecord::new(
                    mount.mount_id.clone(),
                    RemoteId::new("stub-page"),
                    EntityKind::Page,
                    "Stub",
                    "Stub/page.md",
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save stub entity");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    mount.mount_id.clone(),
                    RemoteId::new("recent-page"),
                    FreshnessTier::Hot,
                )
                .opened_at("unix_ms:600000"),
            )
            .expect("save recent freshness");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    mount.mount_id.clone(),
                    RemoteId::new("cold-page"),
                    FreshnessTier::Cold,
                )
                .opened_at("unix_ms:1"),
            )
            .expect("save cold freshness");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    mount.mount_id.clone(),
                    RemoteId::new("stub-page"),
                    FreshnessTier::Hot,
                )
                .opened_at("unix_ms:600000"),
            )
            .expect("save stub freshness");

        let targets = live_mode_local_reconcile_targets_for_mount_at(&store, &mount, 5, 600000)
            .expect("targets");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].remote_id, RemoteId::new("recent-page"));
        assert!(targets[0].path.ends_with("Recent/page.md"));
    }

    #[test]
    fn live_mode_tick_reports_remote_queue_failure() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes.clear();
        let target = live_mode_target("/tmp/Locality/notion/teamspace-home/hello-world/page.md");

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            std::slice::from_ref(&target),
            |_, _| panic!("remote pull should not sync local changes"),
            |_| Err("daemon unreachable".to_string()),
            |_, _| panic!("remote-only tick should not merge local drift"),
        );

        assert!(!report.ok);
        assert_eq!(report.message, "daemon unreachable");
    }

    #[test]
    fn live_mode_tick_queues_remote_only_pending_change_instead_of_pausing() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes = vec![PendingChange {
            mount_id: "notion-main".to_string(),
            entity_id: "remote-page".to_string(),
            title: "Remote Page".to_string(),
            local_path: "Teamspace/Remote Page/page.md".to_string(),
            summary: "remote update available".to_string(),
            state: "remote_update_available".to_string(),
            issue_codes: vec!["remote_changed".to_string()],
            live_mode: sample_live_mode_status(true),
        }];
        let mut pulled = Vec::new();

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            &[live_mode_target(
                "/tmp/Locality/notion/another-page/page.md",
            )],
            |_, _| panic!("remote-only change should not run a local push"),
            |target| {
                pulled.push(target.clone());
                Ok(())
            },
            |_, _| panic!("remote-only change should not merge local drift"),
        );

        assert!(report.ok);
        assert_eq!(report.message, "Live Mode queued 1 remote update.");
        assert_eq!(pulled.len(), 1);
        assert_eq!(pulled[0].mount_id, MountId::new("notion-main"));
        assert_eq!(pulled[0].remote_id, RemoteId::new("remote-page"));
        assert!(pulled[0].path.ends_with("Teamspace/Remote Page/page.md"));
    }

    #[test]
    fn live_mode_tick_queues_remote_only_pending_change_before_review_work() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes = vec![
            PendingChange {
                mount_id: "notion-main".to_string(),
                entity_id: "local-review-page".to_string(),
                title: "Local Review Page".to_string(),
                local_path: "Teamspace/Local Review Page/page.md".to_string(),
                summary: "needs review: large deletion".to_string(),
                state: "needs_review".to_string(),
                issue_codes: vec!["large_deletion".to_string()],
                live_mode: sample_live_mode_status(true),
            },
            PendingChange {
                mount_id: "notion-main".to_string(),
                entity_id: "remote-page".to_string(),
                title: "Remote Page".to_string(),
                local_path: "Teamspace/Remote Page/page.md".to_string(),
                summary: "remote update available".to_string(),
                state: "needs_review".to_string(),
                issue_codes: vec!["remote_changed".to_string()],
                live_mode: sample_live_mode_status(true),
            },
        ];
        let mut pulled = Vec::new();

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            &[],
            |_, _| panic!("remote-only change should not run a local push"),
            |target| {
                pulled.push(target.clone());
                Ok(())
            },
            |_, _| panic!("remote-only change should not merge local drift"),
        );

        assert!(report.ok);
        assert_eq!(report.message, "Live Mode queued 1 remote update.");
        assert_eq!(pulled.len(), 1);
        assert_eq!(pulled[0].mount_id, MountId::new("notion-main"));
        assert_eq!(pulled[0].remote_id, RemoteId::new("remote-page"));
        assert!(pulled[0].path.ends_with("Teamspace/Remote Page/page.md"));
    }

    #[test]
    fn live_mode_tick_pauses_for_remote_deleted_pending_change() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes = vec![PendingChange {
            mount_id: "notion-main".to_string(),
            entity_id: "remote-page".to_string(),
            title: "Remote Page".to_string(),
            local_path: "Teamspace/Remote Page/page.md".to_string(),
            summary: "remote page deleted or unavailable".to_string(),
            state: "needs_review".to_string(),
            issue_codes: vec!["remote_deleted".to_string()],
            live_mode: sample_live_mode_status(true),
        }];

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            &[live_mode_target(
                "/tmp/Locality/notion/another-page/page.md",
            )],
            |_, _| panic!("remote delete should not push"),
            |_| panic!("remote delete should not fast-forward"),
            |_, _| panic!("remote delete should not merge remote drift"),
        );

        assert!(!report.ok);
        assert_eq!(
            report.message,
            "Live Mode paused for `Remote Page`: remote page deleted or unavailable."
        );
    }

    #[test]
    fn live_mode_merge_remote_drift_keeps_non_overlapping_local_and_remote_changes() {
        let frontmatter = "title: Roadmap\n".to_string();
        let local = render_canonical_markdown(&CanonicalDocument::new(
            frontmatter.clone(),
            "Intro.\n\nLocal middle.\n\nFooter.\n".to_string(),
        ));
        let remote = CanonicalDocument::new(
            frontmatter,
            "Remote intro.\n\nOld middle.\n\nFooter.\n".to_string(),
        );

        let merged = live_mode_merge_remote_drift_markdown(
            &local,
            "Intro.\n\nOld middle.\n\nFooter.\n",
            &remote,
        );

        assert!(!merged.has_conflicts);
        assert!(!has_unresolved_conflict_markers(&merged.markdown));
        assert!(merged.markdown.contains("Remote intro."));
        assert!(merged.markdown.contains("Local middle."));
    }

    #[test]
    fn live_mode_merge_remote_drift_materializes_conflict_markers_for_overlapping_changes() {
        let frontmatter = "title: Roadmap\n".to_string();
        let local = render_canonical_markdown(&CanonicalDocument::new(
            frontmatter.clone(),
            "Local intro.\n\nFooter.\n".to_string(),
        ));
        let remote = CanonicalDocument::new(frontmatter, "Remote intro.\n\nFooter.\n".to_string());

        let merged =
            live_mode_merge_remote_drift_markdown(&local, "Base intro.\n\nFooter.\n", &remote);

        assert!(merged.has_conflicts);
        assert!(has_unresolved_conflict_markers(&merged.markdown));
        assert!(merged.markdown.contains("Local intro."));
        assert!(merged.markdown.contains("Remote intro."));
    }

    #[test]
    fn live_mode_local_reconcile_is_throttled_per_target() {
        let mut times = BTreeMap::new();
        let now = Instant::now();
        let page = PathBuf::from("/tmp/Locality/notion/teamspace-home/hello-world/page.md");
        let other = PathBuf::from("/tmp/Locality/notion/teamspace-home/other/page.md");

        assert!(live_mode_should_reconcile_local_target_for_key(
            &mut times,
            page.clone(),
            now,
            Duration::from_secs(5),
        ));
        assert!(!live_mode_should_reconcile_local_target_for_key(
            &mut times,
            page.clone(),
            now + Duration::from_secs(1),
            Duration::from_secs(5),
        ));
        assert!(live_mode_should_reconcile_local_target_for_key(
            &mut times,
            other,
            now + Duration::from_secs(1),
            Duration::from_secs(5),
        ));
        assert!(live_mode_should_reconcile_local_target_for_key(
            &mut times,
            page,
            now + Duration::from_secs(5),
            Duration::from_secs(5),
        ));
    }

    #[test]
    fn live_mode_remote_pull_scan_is_throttled_per_mount() {
        let mut times = BTreeMap::new();
        let now = Instant::now();
        let mount = MountId::new("notion-main");

        assert!(live_mode_remote_pull_scan_is_due_for_key(
            &mut times,
            mount.clone(),
            now,
            Duration::from_secs(5),
        ));
        assert!(!live_mode_remote_pull_scan_is_due_for_key(
            &mut times,
            mount.clone(),
            now + Duration::from_secs(1),
            Duration::from_secs(5),
        ));
        assert!(live_mode_remote_pull_scan_is_due_for_key(
            &mut times,
            mount,
            now + Duration::from_secs(5),
            Duration::from_secs(5),
        ));
    }

    #[test]
    #[ignore = "requires live Notion credentials and LOCALITY_NOTION_LIVE_PARENT_PAGE, or LOCALITY_DESKTOP_LIVE_MODE_E2E_PAGE; mutates then cleans up scratch content"]
    fn live_mode_bidirectional_cloudstorage_markdown_e2e() {
        let context = live_mode_e2e_context();
        let page_path = context.page_path.clone();
        let page_id = context.page_id.clone();
        let api = context.api.clone();
        let state_root = context.state_root.clone();
        let run_id = format!("locality-desktop-live-e2e-{}", unique_suffix());
        let cleanup = LiveModeE2eCleanup {
            api: api.clone(),
            page_id: page_id.clone(),
            run_id: run_id.clone(),
        };

        live_mode_e2e_wait_until("initial page is clean", || {
            live_mode_tick_blocking_at_state_root(&state_root).ok
                && fs::read_to_string(&page_path)
                    .map(|contents| !contents.contains("<<<<<<<"))
                    .unwrap_or(false)
        });

        for index in 1..=3 {
            let marker = format!("{run_id} local-to-cloud {index}");
            live_mode_e2e_append_local_marker(&page_path, &marker);
            live_mode_e2e_wait_until(&format!("local marker {index} reaches Notion"), || {
                live_mode_tick_blocking_at_state_root(&state_root).ok
                    && live_mode_e2e_remote_text(&api, &page_id).contains(&marker)
            });
        }

        for index in 1..=3 {
            let marker = format!("{run_id} cloud-to-local {index}");
            live_mode_e2e_append_remote_marker(&api, &page_id, &marker);
            live_mode_e2e_wait_until(
                &format!("remote marker {index} reaches the local file"),
                || {
                    live_mode_tick_blocking_at_state_root(&state_root).ok
                        && fs::read_to_string(&page_path)
                            .map(|contents| contents.contains(&marker))
                            .unwrap_or(false)
                },
            );
        }

        drop(cleanup);
        live_mode_e2e_wait_until("cleanup reaches the local file", || {
            live_mode_tick_blocking_at_state_root(&state_root).ok
                && fs::read_to_string(&page_path)
                    .map(|contents| !contents.contains(&run_id))
                    .unwrap_or(false)
        });
    }

    #[test]
    fn live_mode_tick_syncs_safe_pending_changes() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes = vec![PendingChange {
            mount_id: "notion-main".to_string(),
            entity_id: "roadmap-page".to_string(),
            title: "Roadmap".to_string(),
            local_path: "Engineering/Roadmap/page.md".to_string(),
            summary: "local edits pending review".to_string(),
            state: "safe".to_string(),
            issue_codes: Vec::new(),
            live_mode: sample_live_mode_status(false),
        }];
        let mut synced = Vec::new();

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            &[live_mode_target("/tmp/Locality/notion/Other/page.md")],
            |change, target| {
                synced.push((change.title.clone(), target.to_path_buf()));
                Ok(())
            },
            |_| panic!("pending changes should not queue a remote fast-forward"),
            |_, _| panic!("safe pending changes should not merge remote drift"),
        );

        assert!(report.ok);
        assert_eq!(report.message, "Live Mode synced 1 pending change.");
        assert_eq!(synced.len(), 1);
        assert_eq!(synced[0].0, "Roadmap");
        assert!(synced[0].1.ends_with("Engineering/Roadmap/page.md"));
    }

    #[test]
    fn live_mode_tick_merges_remote_drift_before_syncing_obvious_case() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes = vec![PendingChange {
            mount_id: "notion-main".to_string(),
            entity_id: "roadmap-page".to_string(),
            title: "Roadmap".to_string(),
            local_path: "Engineering/Roadmap/page.md".to_string(),
            summary: "remote changed while local edits are pending".to_string(),
            state: "needs_review".to_string(),
            issue_codes: vec!["remote_changed_with_local_pending".to_string()],
            live_mode: sample_live_mode_status(true),
        }];
        let mut merged = Vec::new();
        let mut synced = Vec::new();

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            &[live_mode_target("/tmp/Locality/notion/Other/page.md")],
            |change, target| {
                synced.push((change.title.clone(), target.to_path_buf()));
                Ok(())
            },
            |_| panic!("mergeable local+remote drift should not queue remote fast-forward"),
            |change, target| {
                merged.push((change.title.clone(), target.to_path_buf()));
                Ok(LiveModeRemoteDriftMerge::Clean)
            },
        );

        assert!(report.ok);
        assert_eq!(
            report.message,
            "Live Mode merged remote updates and synced 1 pending change."
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(synced.len(), 1);
        assert_eq!(merged[0].0, "Roadmap");
        assert!(merged[0].1.ends_with("Engineering/Roadmap/page.md"));
        assert_eq!(synced[0].1, merged[0].1);
    }

    #[test]
    fn live_mode_tick_keeps_running_after_writing_conflict_markers() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes = vec![PendingChange {
            mount_id: "notion-main".to_string(),
            entity_id: "roadmap-page".to_string(),
            title: "Roadmap".to_string(),
            local_path: "Engineering/Roadmap/page.md".to_string(),
            summary: "remote changed while local edits are pending".to_string(),
            state: "needs_review".to_string(),
            issue_codes: vec!["remote_changed_with_local_pending".to_string()],
            live_mode: sample_live_mode_status(true),
        }];
        let mut merged = Vec::new();

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            &[],
            |_, _| panic!("conflicted local+remote drift should not push"),
            |_| panic!("conflicted local+remote drift should not queue remote fast-forward"),
            |change, target| {
                merged.push((change.title.clone(), target.to_path_buf()));
                Ok(LiveModeRemoteDriftMerge::ConflictMarkersWritten)
            },
        );

        assert!(report.ok);
        assert_eq!(
            report.message,
            "Live Mode pulled remote updates for `Roadmap` and wrote conflict markers for review."
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].0, "Roadmap");
    }

    #[test]
    fn live_mode_tick_pauses_for_review_required_changes() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes = vec![PendingChange {
            mount_id: "notion-main".to_string(),
            entity_id: "launch-plan-page".to_string(),
            title: "Launch Plan".to_string(),
            local_path: "Marketing/Launch Plan/page.md".to_string(),
            summary: "needs review: large deletion".to_string(),
            state: "needs_review".to_string(),
            issue_codes: vec!["large_deletion".to_string()],
            live_mode: sample_live_mode_status(false),
        }];
        let mut calls = 0usize;

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            &[live_mode_target("/tmp/Locality/notion/Other/page.md")],
            |_, _| {
                calls += 1;
                Ok(())
            },
            |_| panic!("review-required changes should not queue remote fast-forward"),
            |_, _| panic!("large review-required changes should not merge remote drift"),
        );

        assert!(!report.ok);
        assert!(report.message.contains("Launch Plan"));
        assert!(report.message.contains("needs review"));
        assert_eq!(calls, 0);
    }

    #[test]
    fn live_mode_summary_hides_stale_disabled_error_without_pending_changes() {
        let record = MountLiveModeRecord::new(MountId::new("notion-main"), true, "1").error(
            "Live Mode could not inspect Notion changes: network unavailable",
            "2",
            "2",
        );

        let summary = super::MountLiveModeSummary::from_record(Some(&record), &[]);

        assert!(!summary.enabled);
        assert_eq!(summary.state, "off");
        assert_eq!(summary.label, "Live Mode off");
        assert_eq!(summary.reason, None);
    }

    #[test]
    fn live_mode_summary_hides_stale_disabled_pause_without_pending_changes() {
        let record = MountLiveModeRecord::new(MountId::new("notion-main"), true, "1").error(
            "Live Mode paused for `Roadmap`: conflict.",
            "2",
            "2",
        );

        let summary = super::MountLiveModeSummary::from_record(Some(&record), &[]);

        assert!(!summary.enabled);
        assert_eq!(summary.state, "off");
        assert_eq!(summary.label, "Live Mode off");
        assert_eq!(summary.reason, None);
    }

    #[test]
    fn live_mode_disabled_remote_drift_pause_can_resume() {
        let record = MountLiveModeRecord::new(MountId::new("notion-main"), true, "1").error(
            "Live Mode paused for `Roadmap`: remote changed while local edits are pending.",
            "2",
            "2",
        );

        assert!(super::live_mode_disabled_pause_can_resume(&record));
    }

    #[test]
    fn live_mode_disabled_non_remote_drift_pause_stays_paused() {
        let record = MountLiveModeRecord::new(MountId::new("notion-main"), true, "1").error(
            "Live Mode paused for `Roadmap`: conflict.",
            "2",
            "2",
        );

        assert!(!super::live_mode_disabled_pause_can_resume(&record));
    }

    #[test]
    fn live_mode_enabled_mount_resumes_stale_remote_drift_pause_for_safe_pending_change() {
        let temp = TestTempDir::new("live-mode-resume-stale-remote-drift");
        let state_root = temp.path();
        let mount_root = state_root.join("notion");
        let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open store");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let relative_path = Path::new("Roadmap/page.md");
        let body = "# Roadmap\n\nOriginal body.\n";
        let mount = MountConfig::new(mount_id.clone(), "notion", &mount_root)
            .projection(ProjectionMode::MacosFileProvider);
        store.save_mount(mount.clone()).expect("save mount");
        store
            .save_mount_live_mode(MountLiveModeRecord::new(mount_id.clone(), true, "1").error(
                "Live Mode paused for `Roadmap`: remote changed while local edits are pending.",
                "2",
                "2",
            ))
            .expect("save paused live mode");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    relative_path,
                )
                .with_hydration(HydrationState::Dirty)
                .with_remote_edited_at("remote-v1"),
            )
            .expect("save entity");
        store
            .save_shadow(
                &mount_id,
                ShadowDocument::from_synced_body(
                    remote_id.clone(),
                    body,
                    7,
                    [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
                )
                .expect("shadow"),
            )
            .expect("save shadow");
        store
            .save_remote_observation(
                RemoteObservationRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    relative_path,
                    "unix_ms:10",
                )
                .with_remote_version(RemoteVersion::new("remote-v1")),
            )
            .expect("save observation");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(mount_id.clone(), remote_id.clone(), FreshnessTier::Hot)
                    .checked_at("unix_ms:10"),
            )
            .expect("save freshness");

        let content_path =
            localityd::virtual_fs::virtual_fs_content_path(state_root, &mount_id, relative_path)
                .expect("content path");
        fs::create_dir_all(content_path.parent().expect("content parent"))
            .expect("create content parent");
        fs::write(&content_path, "# Roadmap\n\nLocal edit.\n").expect("write dirty content");

        let selected = live_mode_enabled_mount(state_root)
            .expect("enabled mount")
            .expect("resumed mount");
        assert_eq!(selected.mount_id, mount_id);
        let record = store
            .get_mount_live_mode(&mount_id)
            .expect("load live mode")
            .expect("live mode record");
        assert!(record.enabled);
        assert_eq!(record.state, MountLiveModeState::Active);
    }

    #[test]
    fn live_mode_transient_failures_remain_enabled_for_retry() {
        let temp = TestTempDir::new("live-mode-transient-failure");
        let mount_id = MountId::new("notion-main");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        store
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                temp.path().join("notion"),
            ))
            .expect("save mount");
        store
            .save_mount_live_mode(MountLiveModeRecord::new(mount_id.clone(), true, "1"))
            .expect("save live mode");
        drop(store);

        record_mount_live_mode_tick_result(
            temp.path(),
            &mount_id,
            &ActionReport {
                ok: false,
                message: "Live Mode could not inspect Notion changes: network unavailable"
                    .to_string(),
            },
        )
        .expect("record transient result");

        let store = SqliteStateStore::open(temp.path().to_path_buf()).expect("reopen store");
        let record = store
            .get_mount_live_mode(&mount_id)
            .expect("load live mode")
            .expect("live mode record");
        assert!(record.enabled);
        assert_eq!(record.state, MountLiveModeState::Active);
        assert_eq!(
            record.last_reason.as_deref(),
            Some("Live Mode could not inspect Notion changes: network unavailable")
        );
    }

    #[test]
    fn live_mode_review_failures_pause_until_user_action() {
        let temp = TestTempDir::new("live-mode-review-failure");
        let mount_id = MountId::new("notion-main");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        store
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                temp.path().join("notion"),
            ))
            .expect("save mount");
        store
            .save_mount_live_mode(MountLiveModeRecord::new(mount_id.clone(), true, "1"))
            .expect("save live mode");
        drop(store);

        record_mount_live_mode_tick_result(
            temp.path(),
            &mount_id,
            &ActionReport {
                ok: false,
                message: "Live Mode paused for `Roadmap`: needs review.".to_string(),
            },
        )
        .expect("record review result");

        let store = SqliteStateStore::open(temp.path().to_path_buf()).expect("reopen store");
        let record = store
            .get_mount_live_mode(&mount_id)
            .expect("load live mode")
            .expect("live mode record");
        assert!(!record.enabled);
        assert_eq!(record.state, MountLiveModeState::Error);
    }

    #[test]
    fn live_mode_syncing_does_not_reenable_disabled_record() {
        let temp = TestTempDir::new("live-mode-syncing-disabled");
        let mount_id = MountId::new("notion-main");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        store
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                temp.path().join("notion"),
            ))
            .expect("save mount");
        store
            .save_mount_live_mode(MountLiveModeRecord::new(mount_id.clone(), true, "1").off("2"))
            .expect("save disabled live mode");
        drop(store);

        let should_continue =
            mark_mount_live_mode_syncing(temp.path(), &mount_id).expect("mark syncing");

        assert!(!should_continue);
        let store = SqliteStateStore::open(temp.path().to_path_buf()).expect("reopen store");
        let record = store
            .get_mount_live_mode(&mount_id)
            .expect("load live mode")
            .expect("live mode record");
        assert!(!record.enabled);
        assert_eq!(record.state, MountLiveModeState::Off);
    }

    #[test]
    fn live_mode_tick_result_does_not_reenable_disabled_record() {
        let temp = TestTempDir::new("live-mode-result-disabled");
        let mount_id = MountId::new("notion-main");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        store
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                temp.path().join("notion"),
            ))
            .expect("save mount");
        store
            .save_mount_live_mode(MountLiveModeRecord::new(mount_id.clone(), true, "1").off("2"))
            .expect("save disabled live mode");
        drop(store);

        record_mount_live_mode_tick_result(
            temp.path(),
            &mount_id,
            &ActionReport {
                ok: true,
                message: "Live Mode checked for changes.".to_string(),
            },
        )
        .expect("record disabled result");

        let store = SqliteStateStore::open(temp.path().to_path_buf()).expect("reopen store");
        let record = store
            .get_mount_live_mode(&mount_id)
            .expect("load live mode")
            .expect("live mode record");
        assert!(!record.enabled);
        assert_eq!(record.state, MountLiveModeState::Off);
        assert_eq!(record.last_reason, None);
    }

    #[test]
    fn live_mode_remote_pull_candidates_include_only_hydrated_pages() {
        let mount_id = MountId::new("notion-main");
        let mount = MountConfig::new(mount_id.clone(), "notion", "/tmp/Locality/notion");
        let mut store = InMemoryStateStore::default();
        store.save_mount(mount.clone()).expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("page-hydrated"),
                    EntityKind::Page,
                    "Hello World",
                    "teamspace-home/hello-world/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save hydrated page");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    mount_id.clone(),
                    RemoteId::new("page-hydrated"),
                    FreshnessTier::Hot,
                )
                .opened_at(activity_timestamp())
                .checked_at("unix_ms:1"),
            )
            .expect("save active freshness");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("page-stub"),
                    EntityKind::Page,
                    "Stub",
                    "teamspace-home/stub/page.md",
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save stub page");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id,
                    RemoteId::new("db"),
                    EntityKind::Database,
                    "Database",
                    "teamspace-home/database",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save database");

        let candidates = live_mode_remote_pull_candidates(&store, &mount).expect("candidates");

        assert_eq!(
            candidates
                .into_iter()
                .map(|candidate| candidate.path)
                .collect::<Vec<_>>(),
            vec![PathBuf::from(
                "/tmp/Locality/notion/teamspace-home/hello-world/page.md"
            )]
        );
    }

    #[test]
    fn live_mode_remote_pull_candidates_skip_inactive_hydrated_pages() {
        let mount_id = MountId::new("notion-main");
        let mount = MountConfig::new(mount_id.clone(), "notion", "/tmp/Locality/notion");
        let mut store = InMemoryStateStore::default();
        store.save_mount(mount.clone()).expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("inactive-page"),
                    EntityKind::Page,
                    "Inactive",
                    "teamspace-home/inactive/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save hydrated page");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    mount_id,
                    RemoteId::new("inactive-page"),
                    FreshnessTier::Warm,
                )
                .opened_at("unix_ms:1")
                .checked_at("unix_ms:2"),
            )
            .expect("save stale freshness");

        let candidates = live_mode_remote_pull_candidates(&store, &mount).expect("candidates");

        assert!(candidates.is_empty());
    }

    #[test]
    fn live_mode_remote_pull_candidates_include_pending_remote_hints_without_poll_delay() {
        let mount_id = MountId::new("notion-main");
        let mount = MountConfig::new(mount_id.clone(), "notion", "/tmp/Locality/notion");
        let mut store = InMemoryStateStore::default();
        store.save_mount(mount.clone()).expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    RemoteId::new("hinted-page"),
                    EntityKind::Page,
                    "Hinted",
                    "teamspace-home/hinted/page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save hydrated page");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    mount_id,
                    RemoteId::new("hinted-page"),
                    FreshnessTier::Hot,
                )
                .opened_at(activity_timestamp())
                .checked_at(activity_timestamp())
                .remote_hint_pending(true),
            )
            .expect("save pending freshness");

        let candidates = live_mode_remote_pull_candidates(&store, &mount).expect("candidates");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].remote_id, RemoteId::new("hinted-page"));
        assert!(
            candidates[0]
                .path
                .ends_with("teamspace-home/hinted/page.md")
        );
    }

    #[test]
    fn extracts_id_from_notion_pretty_workspace_url() {
        assert_eq!(
            notion_id_from_url(
                "https://app.notion.com/p/codeflash/Initial-Idea-37b3ac0ebb88802cbcf4d53c9cfc4972",
            )
            .as_deref(),
            Some("37b3ac0ebb88802cbcf4d53c9cfc4972")
        );
    }

    #[test]
    fn extracts_id_before_query_string() {
        assert_eq!(
            notion_id_from_url(
                "https://www.notion.so/Initial-Idea-37b3ac0ebb88802cbcf4d53c9cfc4972?pvs=4"
            )
            .as_deref(),
            Some("37b3ac0ebb88802cbcf4d53c9cfc4972")
        );
    }

    #[test]
    fn exact_notion_entry_match_accepts_hyphenated_api_ids() {
        assert!(exact_notion_entry_matches(
            &RemoteId::new("3903ac0e-bb88-80d6-ad20-f29c8156e453"),
            "3903ac0ebb8880d6ad20f29c8156e453",
        ));
    }

    #[test]
    fn notion_locator_message_rejects_github_commit_urls() {
        let message = unsupported_notion_locator_url_message(
            "https://github.com/codeflash-ai/locality/commit/15e6dedcfd04d1cdb22df006b66a90dd4ab3753c",
        )
        .expect("unsupported URL message");

        assert!(message.contains("GitHub"));
        assert!(message.contains("Notion pages and databases only"));
    }

    #[test]
    fn notion_locator_message_allows_notion_urls() {
        assert_eq!(
            unsupported_notion_locator_url_message(
                "https://app.notion.com/p/codeflash/Initial-Idea-37b3ac0ebb88802cbcf4d53c9cfc4972",
            ),
            None
        );
    }

    #[test]
    fn mount_validation_rejects_file_paths() {
        let temp = TestTempDir::new("file-path");
        let file = temp.path().join("notion.md");
        fs::write(&file, "not a folder").expect("write test file");

        let error =
            validate_mount_root(&file, &temp.path().join(".loc")).expect_err("file rejected");

        assert!(error.contains("not a file"));
    }

    #[test]
    fn mount_validation_rejects_state_directory() {
        let temp = TestTempDir::new("state-dir");
        let state_root = temp.path().join(".loc");
        fs::create_dir_all(&state_root).expect("create state dir");

        let error = validate_mount_root(&state_root, &state_root).expect_err("state dir rejected");

        assert!(error.contains("outside the Locality state directory"));
    }

    #[test]
    fn mount_validation_accepts_new_child_under_existing_parent() {
        let temp = TestTempDir::new("new-child");
        let root = temp.path().join("Notion");

        validate_mount_root(&root, &temp.path().join(".loc")).expect("valid child path");
    }

    #[test]
    fn mount_validation_rejects_existing_read_only_directory() {
        let temp = TestTempDir::new("read-only-mount-root");
        let root = temp.path().join("Notion");
        fs::create_dir_all(&root).expect("create mount root");

        let original_permissions = fs::metadata(&root)
            .expect("read mount metadata")
            .permissions();
        let mut read_only_permissions = original_permissions.clone();
        read_only_permissions.set_readonly(true);
        fs::set_permissions(&root, read_only_permissions).expect("make mount read-only");
        let result = validate_mount_root(&root, &temp.path().join(".loc"));
        fs::set_permissions(&root, original_permissions).expect("restore mount permissions");

        assert_eq!(
            result.expect_err("ordinary read-only mount root rejected"),
            format!("Selected folder is read-only: {}", root.display())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_desktop_mount_accepts_read_only_file_provider_mount_point() {
        let temp = TestTempDir::new("desktop-read-only-file-provider-root");
        let provider_root = temp.path().join("Locality");
        let root = provider_root.join("notion");
        fs::create_dir_all(&root).expect("create provider mount point");

        let original_permissions = fs::metadata(&root)
            .expect("read provider mount point metadata")
            .permissions();
        let mut read_only_permissions = original_permissions.clone();
        read_only_permissions.set_readonly(true);
        fs::set_permissions(&root, read_only_permissions).expect("make mount point read-only");
        assert!(
            fs::metadata(&root)
                .expect("read updated mount point metadata")
                .permissions()
                .readonly()
        );

        let result = super::validate_desktop_mount_root_with_macos_provider_roots(
            &root,
            &temp.path().join(".loc"),
            &ProjectionMode::MacosFileProvider,
            std::slice::from_ref(&provider_root),
        );

        fs::set_permissions(&root, original_permissions).expect("restore mount point permissions");
        result.expect("provider-owned mount point should not require a POSIX write bit");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_file_provider_mount_rejects_parent_traversal_outside_provider_root() {
        let temp = TestTempDir::new("desktop-file-provider-parent-traversal");
        let provider_root = temp.path().join("Locality");
        let outside_root = temp.path().join("outside");
        fs::create_dir_all(&provider_root).expect("create provider root");
        fs::create_dir_all(&outside_root).expect("create outside root");
        let traversing_root = provider_root.join("..").join("outside");

        let error = super::validate_macos_file_provider_mount_root(
            &traversing_root,
            &temp.path().join(".loc"),
            std::slice::from_ref(&provider_root),
        )
        .expect_err("parent traversal outside provider root rejected");

        assert!(error.contains("inside the Locality File Provider root"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_file_provider_mount_rejects_symlink_outside_provider_root() {
        use std::os::unix::fs::symlink;

        let temp = TestTempDir::new("desktop-file-provider-symlink-escape");
        let provider_root = temp.path().join("Locality");
        let outside_root = temp.path().join("outside");
        fs::create_dir_all(&provider_root).expect("create provider root");
        fs::create_dir_all(&outside_root).expect("create outside root");
        let symlink_root = provider_root.join("escape");
        symlink(&outside_root, &symlink_root).expect("create escaping symlink");

        let error = super::validate_macos_file_provider_mount_root(
            &symlink_root,
            &temp.path().join(".loc"),
            std::slice::from_ref(&provider_root),
        )
        .expect_err("symlink outside provider root rejected");

        assert!(error.contains("inside the Locality File Provider root"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_file_provider_mount_rejects_direct_path_under_symlinked_provider_root_target() {
        use std::os::unix::fs::symlink;

        let temp = TestTempDir::new("desktop-file-provider-symlinked-provider-direct-target");
        let provider_root = temp.path().join("Locality-Locality");
        let outside_root = temp.path().join("outside");
        let outside_mount = outside_root.join("notion");
        fs::create_dir_all(&outside_mount).expect("create outside mount root");
        symlink(&outside_root, &provider_root).expect("create symlinked provider root");

        let error = super::validate_macos_file_provider_mount_root(
            &outside_mount,
            &temp.path().join(".loc"),
            std::slice::from_ref(&provider_root),
        )
        .expect_err("direct path under symlinked provider target rejected");

        assert!(error.contains("inside the Locality File Provider root"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_file_provider_mount_rejects_path_spelled_through_symlinked_provider_root() {
        use std::os::unix::fs::symlink;

        let temp = TestTempDir::new("desktop-file-provider-symlinked-provider-path");
        let provider_root = temp.path().join("Locality-Locality");
        let outside_root = temp.path().join("outside");
        let outside_mount = outside_root.join("notion");
        fs::create_dir_all(&outside_mount).expect("create outside mount root");
        symlink(&outside_root, &provider_root).expect("create symlinked provider root");
        let mount_through_provider = provider_root.join("notion");

        let error = super::validate_macos_file_provider_mount_root(
            &mount_through_provider,
            &temp.path().join(".loc"),
            std::slice::from_ref(&provider_root),
        )
        .expect_err("path spelled through symlinked provider root rejected");

        assert!(error.contains("inside the Locality File Provider root"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_file_provider_mount_rejects_symlink_followed_by_parent_traversal() {
        use std::os::unix::fs::symlink;

        let temp = TestTempDir::new("desktop-file-provider-symlink-parent-traversal");
        let provider_root = temp.path().join("Locality");
        let outside_nested = temp.path().join("outside").join("nested");
        let outside_mount = temp.path().join("outside").join("mount");
        fs::create_dir_all(&provider_root).expect("create provider root");
        fs::create_dir_all(&outside_nested).expect("create outside symlink target");
        fs::create_dir_all(&outside_mount).expect("create escaped mount root");
        let symlink_root = provider_root.join("escape");
        symlink(&outside_nested, &symlink_root).expect("create escaping symlink");
        let traversing_root = symlink_root.join("..").join("mount");

        let error = super::validate_macos_file_provider_mount_root(
            &traversing_root,
            &temp.path().join(".loc"),
            std::slice::from_ref(&provider_root),
        )
        .expect_err("symlink plus parent traversal rejected");

        assert!(error.contains("inside the Locality File Provider root"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_file_provider_mount_rejects_file_root() {
        let temp = TestTempDir::new("desktop-file-provider-file-root");
        let provider_root = temp.path().join("Locality");
        let file_root = provider_root.join("notion");
        fs::create_dir_all(&provider_root).expect("create provider root");
        fs::write(&file_root, "not a folder").expect("create file root");

        let error = super::validate_macos_file_provider_mount_root(
            &file_root,
            &temp.path().join(".loc"),
            std::slice::from_ref(&provider_root),
        )
        .expect_err("file mount root rejected");

        assert_eq!(
            error,
            format!("Choose a folder path, not a file: {}", file_root.display())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_file_provider_mount_rejects_file_as_nearest_ancestor() {
        let temp = TestTempDir::new("desktop-file-provider-file-ancestor");
        let provider_root = temp.path().join("Locality");
        let file_parent = provider_root.join("notion");
        let child_root = file_parent.join("child");
        fs::create_dir_all(&provider_root).expect("create provider root");
        fs::write(&file_parent, "not a folder").expect("create file parent");

        let error = super::validate_macos_file_provider_mount_root(
            &child_root,
            &temp.path().join(".loc"),
            std::slice::from_ref(&provider_root),
        )
        .expect_err("file nearest ancestor rejected");

        assert_eq!(
            error,
            format!("Mount parent is not a folder: {}", file_parent.display())
        );
    }

    #[test]
    fn mount_access_root_returns_mount_point_for_virtual_projection() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-main",
        )
        .projection(ProjectionMode::LinuxFuse);

        assert_eq!(
            super::mount_access_root(&mount),
            std::path::PathBuf::from("/tmp/Locality/notion-main")
        );
    }

    #[test]
    fn plain_mount_access_root_stays_at_mount_root() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/Locality")
            .projection(ProjectionMode::PlainFiles);

        assert_eq!(
            super::mount_access_root(&mount),
            std::path::PathBuf::from("/tmp/Locality")
        );
    }

    #[test]
    fn source_backup_exports_current_files_manifest_and_readme() {
        let temp = TestTempDir::new("source-backup");
        let state_root = temp.path().join("state");
        let source_root = temp.path().join("source");
        let backup_root = temp.path().join("backups");
        fs::create_dir_all(source_root.join("Engineering Wiki/Standups"))
            .expect("create source tree");
        fs::write(
            source_root.join("Engineering Wiki/Standups/page.md"),
            "# Standups\n\nToday we shipped backup export.\n",
        )
        .expect("write source page");
        fs::write(source_root.join("README.md"), "# Source\n").expect("write source readme");

        let mut store = SqliteStateStore::open(state_root).expect("open state store");
        store
            .save_mount(MountConfig::new(
                MountId::new("notion-main"),
                "notion",
                &source_root,
            ))
            .expect("save mount");

        let export = super::export_source_backup_to_root(&store, &backup_root, "notion-main")
            .expect("export backup");

        assert!(
            export
                .directory
                .starts_with(backup_root.join("notion-main"))
        );
        assert_eq!(
            fs::read_to_string(
                export
                    .directory
                    .join("files/Engineering Wiki/Standups/page.md")
            )
            .expect("read exported page"),
            "# Standups\n\nToday we shipped backup export.\n"
        );
        assert_eq!(
            fs::read_to_string(export.directory.join("files/README.md"))
                .expect("read exported readme"),
            "# Source\n"
        );
        assert!(export.directory.join("README.md").is_file());
        let manifest: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(export.directory.join("manifest.json"))
                .expect("read backup manifest"),
        )
        .expect("parse backup manifest");
        assert_eq!(manifest["mountId"], "notion-main");
        assert_eq!(manifest["connector"], "notion");
        assert_eq!(manifest["connectorName"], "Notion");
        assert_eq!(manifest["fileCount"], 2);
        assert_eq!(
            manifest["skipped"].as_array().expect("skipped array").len(),
            0
        );
        assert!(
            fs::read_to_string(export.directory.join("README.md"))
                .expect("read backup readme")
                .contains("Editing this backup does not sync back to Notion.")
        );
    }

    #[test]
    fn windows_cloud_files_mount_access_root_stays_at_sync_root() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/Locality")
            .projection(ProjectionMode::WindowsCloudFiles);

        assert_eq!(
            super::mount_access_root(&mount),
            std::path::PathBuf::from("/tmp/Locality")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_file_provider_mount_access_root_uses_stored_cloudstorage_root() {
        let root = super::macos_cloud_storage_dir()
            .join("Locality")
            .join("notion");
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", &root)
            .projection(ProjectionMode::MacosFileProvider);

        assert_eq!(super::mount_access_root(&mount), root);
        assert!(super::mount_root_exists_for_desktop_summary(
            &mount,
            &mount.root
        ));
    }

    #[test]
    fn missing_macos_file_provider_reveal_keeps_requested_path_target() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/Users/test/Library/CloudStorage/Locality/notion",
        )
        .projection(ProjectionMode::MacosFileProvider);

        assert_eq!(
            super::missing_virtual_reveal_action(&mount),
            super::MissingVirtualRevealAction::RevealRequestedPath
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reveal_materializes_visible_file_from_content_cache() {
        let temp = TestTempDir::new("reveal-cache");
        let cache = temp.path().join("cache/page.md");
        let visible = temp
            .path()
            .join("CloudStorage/Locality/notion/Page/page.md");
        fs::create_dir_all(cache.parent().expect("cache parent")).expect("create cache parent");
        fs::write(&cache, "# Page\n\nCached body").expect("write cache");

        super::write_visible_file_from_cache_for_reveal(&cache, &visible)
            .expect("materialize visible");

        assert_eq!(
            fs::read_to_string(&visible).expect("read visible"),
            "# Page\n\nCached body"
        );
    }

    #[test]
    fn missing_non_macos_virtual_reveal_opens_projection_root() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/Locality")
            .projection(ProjectionMode::LinuxFuse);

        assert_eq!(
            super::missing_virtual_reveal_action(&mount),
            super::MissingVirtualRevealAction::OpenProjectionRoot
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_mount_summary_without_mount_reports_cloud_files_projection() {
        assert_eq!(
            super::mount_summary(None, Path::new("."), None, None, None).projection,
            "Windows Cloud Files"
        );
        assert_eq!(
            super::desktop_projection_mode(),
            ProjectionMode::WindowsCloudFiles
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_default_notion_mount_root_is_mount_point_under_shared_root() {
        let home = super::home_dir().expect("home dir");

        assert_eq!(
            super::default_notion_mount_root(),
            home.join("Locality").join("notion")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_default_notion_access_root_is_mount_point() {
        assert_eq!(
            super::default_notion_access_root(),
            super::default_notion_mount_root()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_mount_summary_without_mount_reports_linux_fuse_projection() {
        assert_eq!(
            super::mount_summary(None, Path::new("."), None, None, None).projection,
            "Linux FUSE"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_desktop_mount_normalizes_selected_shared_root_to_mount_point() {
        let home = super::home_dir().expect("home dir");
        let selected = home.join("Locality");

        assert_eq!(
            super::resolve_desktop_mount_root(&selected.display().to_string())
                .expect("resolve mount root"),
            home.join("Locality").join("notion")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_desktop_mount_resolves_bare_name_under_cloudstorage() {
        let root = super::resolve_desktop_mount_root("Notion").expect("resolve mount root");

        assert_eq!(
            root,
            super::macos_locality_cloud_storage_root().join("Notion")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_desktop_mount_rejects_paths_outside_cloudstorage() {
        let temp = TestTempDir::new("desktop-mount-outside-cloudstorage");
        let root = temp.path().join("Notion");

        let error = super::validate_desktop_mount_root(
            &root,
            &temp.path().join(".loc"),
            &locality_store::ProjectionMode::MacosFileProvider,
        )
        .expect_err("non-CloudStorage path rejected");

        assert!(error.contains("inside the Locality File Provider root"));
    }

    #[test]
    fn file_provider_unavailable_error_is_recoverable_after_access_change() {
        assert!(super::recoverable_macos_file_provider_activation_error(
            "Could not register macOS File Provider: The application cannot be used right now."
        ));
        assert!(super::recoverable_macos_file_provider_activation_error(
            "Could not open macOS File Provider domain `loc`: The Locality File Provider is registered but not enabled."
        ));
        assert!(!super::recoverable_macos_file_provider_activation_error(
            "Could not load the top-level Notion folder"
        ));
    }

    #[test]
    fn workspace_mount_onboarding_treats_cloudstorage_delay_as_recoverable_after_approval() {
        assert!(super::recoverable_macos_file_provider_activation_error(
            "Could not open macOS File Provider domain `loc`: locality-file-providerctl did not return a CloudStorage URL"
        ));
        assert!(super::recoverable_macos_file_provider_activation_error(
            "Could not open macOS File Provider domain `loc`: File Provider domain loc exists but macOS has not created /Users/test/Library/CloudStorage/Locality"
        ));
    }

    #[test]
    fn file_provider_enablement_report_distinguishes_registration_and_approval() {
        let missing = super::classify_macos_file_provider_enablement(None, None, Ok(None));
        assert_eq!(missing.state, "not_registered");

        let disabled = super::classify_macos_file_provider_enablement(
            Some(false),
            Some(PathBuf::from("/Users/test/Library/CloudStorage/Locality")),
            Ok(None),
        );
        assert_eq!(disabled.state, "needs_finder_enable");
        assert_eq!(
            disabled.path.as_deref(),
            Some("/Users/test/Library/CloudStorage/Locality")
        );
    }

    #[test]
    fn file_provider_domain_status_reads_helper_url_while_disabled() {
        let helper_report = serde_json::json!({
            "domains": [{
                "identifier": "loc",
                "userEnabled": false,
                "url": "/Users/test/Library/CloudStorage/LocalityPromptTest"
            }]
        });

        let (user_enabled, path) = super::macos_file_provider_domain_status(&helper_report);

        assert_eq!(user_enabled, Some(false));
        assert_eq!(
            path,
            Some(PathBuf::from(
                "/Users/test/Library/CloudStorage/LocalityPromptTest"
            ))
        );
    }

    #[test]
    fn file_provider_enablement_report_waits_for_enabled_root() {
        let report = super::classify_macos_file_provider_enablement(
            Some(true),
            Some(PathBuf::from("/Users/test/Library/CloudStorage/Locality")),
            Ok(None),
        );

        assert_eq!(report.state, "waiting_for_root");
        assert!(report.message.contains("Finishing"));
    }

    #[test]
    fn file_provider_enablement_report_is_ready_only_with_resolved_root() {
        let root = PathBuf::from("/Users/test/Library/CloudStorage/Locality");
        let report = super::classify_macos_file_provider_enablement(
            Some(true),
            Some(root.clone()),
            Ok(Some(root.clone())),
        );

        assert_eq!(report.state, "ready");
        assert_eq!(report.path.as_deref(), root.to_str());
    }

    #[test]
    fn file_provider_enablement_report_exposes_unavailable_errors() {
        let report = super::classify_macos_file_provider_enablement(
            Some(true),
            None,
            Err("File Provider helper unavailable".to_string()),
        );

        assert_eq!(report.state, "unavailable");
        assert_eq!(report.message, "File Provider helper unavailable");
    }

    #[test]
    fn file_provider_reveal_path_prefers_domain_then_existing_parent() {
        let temp = TestTempDir::new("file-provider-reveal");
        let cloud_storage = temp.path().join("Library/CloudStorage");
        fs::create_dir_all(&cloud_storage).expect("create CloudStorage parent");
        let domain = cloud_storage.join("Locality");

        assert_eq!(
            super::macos_file_provider_reveal_path(Some(&domain), &[domain.clone()]),
            Some(cloud_storage)
        );

        fs::create_dir_all(&domain).expect("create domain root");
        assert_eq!(
            super::macos_file_provider_reveal_path(Some(&domain), &[domain.clone()]),
            Some(domain)
        );
    }

    #[test]
    fn macos_file_provider_approval_surface_path_uses_first_existing_candidate() {
        let temp = TestTempDir::new("approval-surface");
        let missing = temp.path().join("Library/CloudStorage/Locality");
        let existing = temp.path().join("Library/CloudStorage/Locality-Locality");
        fs::create_dir_all(&existing).expect("create fallback root");

        let expected = if cfg!(target_os = "macos") {
            Some(existing.clone())
        } else {
            None
        };

        assert_eq!(
            super::macos_file_provider_approval_surface_path(&[missing, existing]),
            expected
        );
    }

    #[test]
    fn macos_workspace_mount_onboarding_state_requires_approval_when_domain_is_disabled() {
        let expected = if cfg!(target_os = "macos") {
            Some(super::MacosWorkspaceMountOnboardingState::ApprovalRequired)
        } else {
            None
        };

        assert_eq!(
            super::macos_workspace_mount_onboarding_state(
                "Could not open macOS File Provider domain `loc`: The Locality File Provider is registered but not enabled.",
                false,
            ),
            expected
        );
        if let Some(state) = expected {
            assert_eq!(state.as_str(), "needs_finder_enable");
        }
    }

    #[test]
    fn macos_workspace_mount_onboarding_state_waits_for_cloudstorage_root_when_enabled() {
        let expected = if cfg!(target_os = "macos") {
            Some(super::MacosWorkspaceMountOnboardingState::WaitingForCloudStorageRoot)
        } else {
            None
        };

        assert_eq!(
            super::macos_workspace_mount_onboarding_state(
                "Could not open macOS File Provider domain `loc`: locality-file-providerctl did not return a CloudStorage URL",
                true,
            ),
            expected
        );
    }

    #[test]
    fn workspace_mount_onboarding_preserves_application_unavailable_guidance() {
        let message = "Could not register macOS File Provider: The application cannot be used right now. The Locality macOS File Provider app or extension is not available to macOS. For local development, run `make install-macos-file-provider`, then reopen Locality and enable the File Provider if macOS asks.";

        assert!(super::recoverable_macos_file_provider_activation_error(
            message
        ));

        let report = super::classify_workspace_mount_onboarding_failure(message);
        assert_eq!(report.state, "failed");
        assert_eq!(report.primary_action, "retry_setup");
        assert_eq!(report.launch_strategy, "none");
        assert_eq!(report.message, message);
    }

    #[test]
    fn workspace_mount_onboarding_curated_message_matches_recoverable_state() {
        assert_eq!(
            super::workspace_mount_onboarding_curated_message(
                super::MacosWorkspaceMountOnboardingState::ApprovalRequired
            ),
            Some("In Finder, click Enable for Locality. Locality will continue automatically.")
        );
        assert_eq!(
            super::workspace_mount_onboarding_curated_message(
                super::MacosWorkspaceMountOnboardingState::WaitingForCloudStorageRoot
            ),
            Some("Locality is still waiting for the CloudStorage folder to appear.")
        );
        assert_eq!(
            super::workspace_mount_onboarding_curated_message(
                super::MacosWorkspaceMountOnboardingState::Created
            ),
            None
        );
    }

    #[test]
    fn workspace_mount_onboarding_refreshes_surfaces_only_for_created_reports() {
        let created = super::workspace_mount_onboarding_report(
            super::MacosWorkspaceMountOnboardingState::Created,
            "Mount ready.",
            super::WorkspaceMountOnboardingPrimaryAction::RetrySetup,
            super::WorkspaceMountOnboardingLaunchStrategy::None,
        );
        let waiting = super::workspace_mount_onboarding_report(
            super::MacosWorkspaceMountOnboardingState::WaitingForCloudStorageRoot,
            "Locality is still waiting for the CloudStorage folder to appear.",
            super::WorkspaceMountOnboardingPrimaryAction::CheckAgain,
            super::WorkspaceMountOnboardingLaunchStrategy::InstructionsOnly,
        );
        let failed = super::workspace_mount_onboarding_report(
            super::MacosWorkspaceMountOnboardingState::Failed,
            "Mount failed.",
            super::WorkspaceMountOnboardingPrimaryAction::RetrySetup,
            super::WorkspaceMountOnboardingLaunchStrategy::None,
        );

        assert!(super::workspace_mount_onboarding_should_refresh_surfaces(
            &created
        ));
        assert!(!super::workspace_mount_onboarding_should_refresh_surfaces(
            &waiting
        ));
        assert!(!super::workspace_mount_onboarding_should_refresh_surfaces(
            &failed
        ));
    }

    #[test]
    fn workspace_mount_onboarding_report_uses_retry_setup_for_failed_state() {
        let report = super::workspace_mount_onboarding_report(
            super::MacosWorkspaceMountOnboardingState::Failed,
            "Could not load the top-level Notion folder.",
            super::WorkspaceMountOnboardingPrimaryAction::RetrySetup,
            super::WorkspaceMountOnboardingLaunchStrategy::None,
        );

        assert_eq!(report.state, "failed");
        assert_eq!(report.primary_action, "retry_setup");
        assert_eq!(report.launch_strategy, "none");
        assert_eq!(
            report.message,
            "Could not load the top-level Notion folder."
        );
    }

    #[test]
    fn workspace_mount_onboarding_report_uses_check_again_for_waiting_root() {
        let report = super::workspace_mount_onboarding_report(
            super::MacosWorkspaceMountOnboardingState::WaitingForCloudStorageRoot,
            "Locality is still waiting for the CloudStorage folder to appear.",
            super::WorkspaceMountOnboardingPrimaryAction::CheckAgain,
            super::WorkspaceMountOnboardingLaunchStrategy::InstructionsOnly,
        );

        assert_eq!(report.state, "waiting_for_cloudstorage_root");
        assert_eq!(report.primary_action, "check_again");
        assert_eq!(report.launch_strategy, "instructions_only");
    }

    #[test]
    fn tray_popover_hides_only_when_tray_window_loses_focus() {
        assert!(should_hide_tray_popover(
            "tray",
            &tauri::WindowEvent::Focused(false)
        ));
        assert!(!should_hide_tray_popover(
            "tray",
            &tauri::WindowEvent::Focused(true)
        ));
        assert!(!should_hide_tray_popover(
            "main",
            &tauri::WindowEvent::Focused(false)
        ));
    }

    #[test]
    fn tray_popover_anchor_uses_tray_icon_rect() {
        let anchor = tray_popover_anchor(
            PhysicalPosition::new(42.0, 12.0),
            Rect {
                position: PhysicalPosition::new(1600_i32, 4_i32).into(),
                size: PhysicalSize::new(24_u32, 22_u32).into(),
            },
        );

        assert_eq!(anchor, PhysicalPosition::new(1612.0, 26.0));
    }

    #[test]
    fn tray_popover_anchor_falls_back_to_click_position_without_rect_size() {
        let click_position = PhysicalPosition::new(42.0, 12.0);
        let anchor = tray_popover_anchor(
            click_position,
            Rect {
                position: PhysicalPosition::new(1600_i32, 4_i32).into(),
                size: PhysicalSize::new(0_u32, 0_u32).into(),
            },
        );

        assert_eq!(anchor, click_position);
    }

    #[test]
    fn tray_monitor_selection_uses_monitor_containing_anchor() {
        let left_work_area = ScreenBounds {
            left: -1440.0,
            top: 24.0,
            right: 0.0,
            bottom: 900.0,
        };
        let right_work_area = ScreenBounds {
            left: 0.0,
            top: 24.0,
            right: 1440.0,
            bottom: 900.0,
        };
        let monitors = [
            MonitorScreenBounds {
                screen: ScreenBounds {
                    left: 0.0,
                    top: 0.0,
                    right: 1440.0,
                    bottom: 900.0,
                },
                work_area: right_work_area,
            },
            MonitorScreenBounds {
                screen: ScreenBounds {
                    left: -1440.0,
                    top: 0.0,
                    right: 0.0,
                    bottom: 900.0,
                },
                work_area: left_work_area,
            },
        ];

        assert_eq!(
            screen_bounds_for_anchor_from_monitors(PhysicalPosition::new(-32.0, 12.0), &monitors),
            Some(left_work_area)
        );
    }

    #[test]
    fn tray_popover_position_stays_inside_right_screen_edge() {
        let position = tray_popover_position(
            PhysicalPosition::new(1424.0, 20.0),
            PhysicalSize::new(360, 520),
            Some(ScreenBounds {
                left: 0.0,
                top: 0.0,
                right: 1440.0,
                bottom: 900.0,
            }),
        );

        assert_eq!(position.x, 1072);
        assert_eq!(position.y, 32);
    }

    #[test]
    fn tray_popover_position_stays_inside_negative_left_screen_edge() {
        let position = tray_popover_position(
            PhysicalPosition::new(-1432.0, 20.0),
            PhysicalSize::new(360, 520),
            Some(ScreenBounds {
                left: -1440.0,
                top: 0.0,
                right: 0.0,
                bottom: 900.0,
            }),
        );

        assert_eq!(position.x, -1432);
        assert_eq!(position.y, 32);
    }

    #[test]
    fn tray_popover_position_moves_above_bottom_edge_when_possible() {
        let position = tray_popover_position(
            PhysicalPosition::new(800.0, 880.0),
            PhysicalSize::new(360, 520),
            Some(ScreenBounds {
                left: 0.0,
                top: 0.0,
                right: 1440.0,
                bottom: 900.0,
            }),
        );

        assert_eq!(position.x, 620);
        assert_eq!(position.y, 348);
    }

    #[test]
    fn tray_icons_have_expected_sizes_and_badges() {
        let ready = tray_icon_image(TrayVisualState::Ready);
        let review = tray_icon_image(TrayVisualState::Review);
        let reconnect = tray_icon_image(TrayVisualState::Reconnect);

        assert_eq!(ready.width(), 36);
        assert_eq!(ready.height(), 36);
        assert_eq!(review.width(), 36);
        assert_eq!(reconnect.height(), 36);
        if tray_icon_should_use_template() {
            assert!(
                !ready.rgba().chunks_exact(4).any(|pixel| {
                    pixel[0] > 240 && pixel[1] > 240 && pixel[2] > 240 && pixel[3] > 200
                }),
                "template tray icons should not carry a white halo that macOS will tint as part of the glyph"
            );
        } else {
            assert!(ready.rgba().chunks_exact(4).any(|pixel| {
                pixel[0] > 240 && pixel[1] > 240 && pixel[2] > 240 && pixel[3] > 200
            }));
        }
        assert!(
            ready
                .rgba()
                .chunks_exact(4)
                .any(|pixel| { pixel[0] < 40 && pixel[1] < 50 && pixel[2] < 70 && pixel[3] > 200 })
        );
        for (x, y) in [(5, 15), (5, 18), (5, 21), (30, 15), (30, 18), (30, 21)] {
            let pixel = tray_icon_pixel(&ready, x, y);
            assert!(
                tray_icon_visible_dark_pixel(pixel, 200),
                "expected short-logo side dot at ({x}, {y}), got {pixel:?}"
            );
        }
        for (x, y) in [(3, 15), (33, 15)] {
            let pixel = tray_icon_pixel(&ready, x, y);
            assert!(
                pixel[3] < 80,
                "expected side dot to stay inside the logo cap span at ({x}, {y}), got {pixel:?}"
            );
        }
        for (x, y) in [(18, 4), (6, 11), (29, 11)] {
            let pixel = tray_icon_pixel(&ready, x, y);
            assert!(
                tray_icon_visible_dark_pixel(pixel, 100),
                "expected short-logo top outline at ({x}, {y}), got {pixel:?}"
            );
        }
        let bottom_body = tray_icon_pixel(&ready, 18, 27);
        assert!(
            tray_icon_visible_dark_pixel(bottom_body, 200),
            "expected short-logo bottom body, got {bottom_body:?}"
        );
        let top_center = tray_icon_pixel(&ready, 18, 9);
        assert!(
            top_center[3] < 80,
            "expected short-logo top interior to stay transparent, got {top_center:?}"
        );
        if tray_icon_should_use_template() {
            let ready_opaque = tray_icon_opaque_pixel_count(&ready);
            assert!(
                tray_icon_opaque_pixel_count(&review) > ready_opaque,
                "review template icon should still add a visible status dot"
            );
            assert!(
                tray_icon_opaque_pixel_count(&reconnect) > ready_opaque,
                "reconnect template icon should still add a visible status dot"
            );
        } else {
            assert!(review.rgba().chunks_exact(4).any(|pixel| {
                pixel[0] > 200
                    && pixel[1] > 100
                    && pixel[1] < 180
                    && pixel[2] < 80
                    && pixel[3] > 200
            }));
            assert!(reconnect.rgba().chunks_exact(4).any(|pixel| {
                pixel[0] > 180 && pixel[1] < 90 && pixel[2] < 90 && pixel[3] > 200
            }));
        }
    }

    fn tray_icon_opaque_pixel_count(image: &tauri::image::Image<'_>) -> usize {
        image
            .rgba()
            .chunks_exact(4)
            .filter(|pixel| pixel[3] > 200)
            .count()
    }

    fn tray_icon_visible_dark_pixel(pixel: [u8; 4], min_alpha: u8) -> bool {
        pixel[0] < 190 && pixel[1] < 190 && pixel[2] < 200 && pixel[3] > min_alpha
    }

    fn tray_icon_pixel(image: &tauri::image::Image<'_>, x: usize, y: usize) -> [u8; 4] {
        let offset = (y * image.width() as usize + x) * 4;
        let rgba = image.rgba();
        [
            rgba[offset],
            rgba[offset + 1],
            rgba[offset + 2],
            rgba[offset + 3],
        ]
    }

    #[cfg(windows)]
    #[test]
    fn windows_single_instance_objects_are_session_scoped_and_null_terminated() {
        assert!(super::WINDOWS_DESKTOP_SINGLE_INSTANCE_MUTEX.starts_with(r"Local\"));
        assert!(super::WINDOWS_DESKTOP_ACTIVATION_EVENT.starts_with(r"Local\"));

        let wide = super::windows_wide_null("Locality");
        let expected = "Locality"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        assert_eq!(wide, expected);
    }

    #[test]
    fn connection_metadata_change_detects_workspace_switches() {
        let previous = test_connection("workspace-1", "Teamspace A");
        let next = test_connection("workspace-2", "Teamspace B");

        assert!(connection_metadata_changed(Some(&previous), Some(&next)));
        assert!(!connection_metadata_changed(
            Some(&previous),
            Some(&previous)
        ));
    }

    #[test]
    fn virtual_projection_refresh_signals_macos_file_provider_visible_containers() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-main",
        )
        .projection(ProjectionMode::MacosFileProvider);

        assert_eq!(
            virtual_projection_refresh_signal_identifiers(&mount),
            vec![
                "root".to_string(),
                "mount:notion-main".to_string(),
                "working-set".to_string(),
            ]
        );
    }

    #[test]
    fn virtual_projection_prefetch_container_identifiers_use_mount_point_root() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/CloudStorage/Locality/notion",
        )
        .projection(ProjectionMode::MacosFileProvider);

        assert_eq!(
            virtual_projection_prefetch_container_identifiers(&mount),
            vec!["root".to_string(), "mount:notion-main".to_string()]
        );
    }

    #[test]
    fn virtual_projection_source_ready_wait_runs_before_provider_registration() {
        assert!(
            virtual_projection_waits_for_mount_point_children_before_registration(
                &ProjectionMode::MacosFileProvider
            )
        );
        assert!(
            virtual_projection_waits_for_mount_point_children_before_registration(
                &ProjectionMode::WindowsCloudFiles
            )
        );
        assert!(
            !virtual_projection_waits_for_mount_point_children_before_registration(
                &ProjectionMode::LinuxFuse
            )
        );
        assert!(
            !virtual_projection_waits_for_mount_point_children_before_registration(
                &ProjectionMode::PlainFiles
            )
        );
    }

    #[test]
    fn virtual_projection_source_ready_timeout_guidance_names_the_connector() {
        assert_eq!(
            virtual_projection_source_ready_timeout_message("granola", "last error"),
            "Granola connected, but Locality could not load any files before mounting. Confirm the source contains readable items, then retry setup. last error"
        );
        assert!(
            virtual_projection_source_ready_timeout_message("notion", "last error")
                .contains("at least one page is selected")
        );
    }

    #[test]
    fn source_destructive_actions_require_mount_scoped_typed_confirmation() {
        let mount_id = MountId::new("granola-main");

        assert!(
            validate_source_action_confirmation("RESET", &mount_id, "RESET granola-main").is_ok()
        );
        assert!(
            validate_source_action_confirmation("DISCONNECT", &mount_id, "DISCONNECT granola-main")
                .is_ok()
        );
        let error = validate_source_action_confirmation("RESET", &mount_id, "RESET")
            .expect_err("mount id is required in confirmation");
        assert_eq!(
            error,
            "Type `RESET granola-main` to confirm this source action."
        );
    }

    #[test]
    fn macos_file_provider_mount_root_health_accepts_provider_item() {
        let details = r#"
            fileproviderItems = (
              {
                displayName = notion;
                isUploaded = 1;
                itemIdentifier = "m:bm90aW9uLW1haW4:bW91bnQ6bm90aW9uLW1haW4";
              }
            );
        "#;

        assert_eq!(
            macos_file_provider_mount_root_health_error(Path::new("/tmp/Locality/notion"), details),
            None
        );
    }

    #[test]
    fn macos_file_provider_mount_root_health_accepts_missing_item_identifier() {
        let details = r#"
            fileproviderItems = (
              {
                displayName = notion;
                isUploaded = 1;
              }
            );
        "#;

        assert_eq!(
            macos_file_provider_mount_root_health_error(Path::new("/tmp/Locality/notion"), details),
            None
        );
    }

    #[test]
    fn macos_file_provider_mount_root_health_rejects_local_upload_error() {
        let details = r#"
            fileproviderItems = (
              {
                displayName = notion;
                uploadingError = "Error Domain=NSCocoaErrorDomain Code=3328";
              }
            );
        "#;

        let error =
            macos_file_provider_mount_root_health_error(Path::new("/tmp/Locality/notion"), details)
                .expect("local upload error should be rejected");

        assert!(error.contains("local-upload error state"));
    }

    #[test]
    fn macos_file_provider_recovery_reason_detects_child_count_mismatch() {
        let details = r#"
            fileproviderItems = (
              {
                childItemCount = 4;
                displayName = notion;
                itemIdentifier = "m:bm90aW9uLW1haW4:bW91bnQ6bm90aW9uLW1haW4";
              }
            );
        "#;

        let reason = macos_file_provider_mount_root_recovery_reason(
            Path::new("/tmp/Locality/notion"),
            details,
            Some(11),
        )
        .expect("child-count mismatch should trigger recovery");

        assert!(reason.contains("reports 4 visible child items"));
        assert!(reason.contains("Locality has 11 current child items"));
    }

    #[test]
    fn macos_file_provider_recovery_reason_accepts_matching_child_count() {
        let details = r#"
            fileproviderItems = (
              {
                childItemCount = 11;
                displayName = notion;
                itemIdentifier = "m:bm90aW9uLW1haW4:bW91bnQ6bm90aW9uLW1haW4";
              }
            );
        "#;

        assert_eq!(
            macos_file_provider_mount_root_recovery_reason(
                Path::new("/tmp/Locality/notion"),
                details,
                Some(11),
            ),
            None
        );
    }

    #[test]
    fn macos_file_provider_missing_mount_root_triggers_recovery() {
        let error = r#"Could not inspect macOS File Provider mount root `/tmp/Locality/granola`: Error Domain=NSPOSIXErrorDomain Code=2 "Couldn't find a file for /tmp/Locality/granola""#;

        assert!(macos_file_provider_mount_root_is_missing(error));
        assert_eq!(
            macos_file_provider_mount_root_inspection_recovery_reason(
                Path::new("/tmp/Locality/granola"),
                Err(error),
                Some(705),
            ),
            Some(error.to_string())
        );
    }

    #[test]
    fn macos_file_provider_mount_root_inspection_accepts_healthy_item() {
        let details = r#"
            fileproviderItems = (
              {
                childItemCount = 705;
                displayName = granola;
              }
            );
        "#;

        assert_eq!(
            macos_file_provider_mount_root_inspection_recovery_reason(
                Path::new("/tmp/Locality/granola"),
                Ok(details),
                Some(705),
            ),
            None
        );
    }

    #[test]
    fn macos_file_provider_child_item_count_parses_evaluate_output() {
        assert_eq!(
            macos_file_provider_child_item_count("childItemCount = 11;\nfilename = notion;"),
            Some(11)
        );
        assert_eq!(
            macos_file_provider_child_item_count("filename = notion;"),
            None
        );
    }

    #[test]
    fn virtual_projection_children_summary_ignores_guidance_files() {
        let report = localityd::virtual_fs::VirtualFsChildrenReport {
            mount_id: "notion-main".to_string(),
            container_identifier: "source:notion".to_string(),
            children: vec![
                virtual_projection_test_item("guidance:AGENTS.md", "AGENTS.md", None),
                virtual_projection_test_item("guidance:CLAUDE.md", "CLAUDE.md", None),
                virtual_projection_test_item("remote-page-1", "product", Some("remote-page-1")),
                virtual_projection_test_item("remote-page-2", "engineering", Some("remote-page-2")),
            ],
        };

        let summary = summarize_virtual_projection_children(&report);

        assert_eq!(summary.total_children, 4);
        assert_eq!(summary.content_children, 2);
        assert_eq!(summary.preview(), "product, engineering");
    }

    #[test]
    fn virtual_projection_children_summary_preview_limits_long_lists() {
        let report = localityd::virtual_fs::VirtualFsChildrenReport {
            mount_id: "notion-main".to_string(),
            container_identifier: "source:notion".to_string(),
            children: (0..8)
                .map(|index| {
                    let name = format!("teamspace-{index}");
                    virtual_projection_test_item(&format!("remote-{index}"), &name, Some("remote"))
                })
                .collect(),
        };

        let summary = summarize_virtual_projection_children(&report);

        assert_eq!(summary.content_children, 8);
        assert_eq!(
            summary.preview(),
            "teamspace-0, teamspace-1, teamspace-2, teamspace-3, teamspace-4, teamspace-5, +2 more"
        );
    }

    fn virtual_projection_test_item(
        identifier: &str,
        filename: &str,
        remote_id: Option<&str>,
    ) -> localityd::virtual_fs::VirtualFsItem {
        localityd::virtual_fs::VirtualFsItem {
            identifier: identifier.to_string(),
            parent_identifier: Some("source:notion".to_string()),
            filename: filename.to_string(),
            kind: localityd::virtual_fs::VirtualFsItemKind::Folder,
            read_only: false,
            entity_kind: None,
            remote_id: remote_id.map(str::to_string),
            path: filename.to_string(),
            hydration: None,
            content_type: "public.folder".to_string(),
            remote_edited_at: None,
            materialized_path: None,
            byte_size: None,
        }
    }

    #[test]
    fn push_action_message_explains_dangerous_confirmation() {
        assert_eq!(
            push_action_message("confirm_dangerous_plan", false, None),
            "This push needs review because it may move, archive, or touch a large amount of Notion content. Open Review Push to approve it."
        );
        assert_eq!(
            push_action_message("confirm_dangerous_plan", false, Some("custom")),
            "custom"
        );
    }

    #[test]
    fn push_action_message_explains_remote_changed_recovery() {
        let message = push_action_message(
            "apply_failed",
            false,
            Some(
                "guardrail blocked push: remote entity `abc` changed since last sync (expected remote_edited_at `2026-06-17T05:45:00.000Z`, found `2026-06-17T06:21:00.000Z`)",
            ),
        );

        assert!(message.contains("Pull latest"));
        assert!(message.contains("resolve any conflict markers"));
    }

    #[test]
    fn failed_push_summary_explains_remote_changed_recovery() {
        let message = failed_push_summary(
            "guardrail blocked push: remote entity `abc` changed since last sync (expected remote_edited_at `2026-06-17T05:45:00.000Z`, found `2026-06-17T06:21:00.000Z`)",
        );

        assert!(message.contains("Notion changed since last sync"));
        assert!(message.contains("Pull latest"));
    }

    #[test]
    fn notion_access_lost_messages_explain_selected_access_recovery() {
        let raw = "io error: notion api returned HTTP 404 Not Found: {\"object\":\"error\",\"status\":404,\"code\":\"object_not_found\",\"message\":\"Could not find page with ID: page-1. Make sure the relevant pages and databases are shared with your integration \\\"Locality\\\".\"}";

        assert!(is_notion_access_lost_message(raw));
        assert!(pull_error_message(raw).contains("outside the current selected access"));
        assert!(failed_push_summary(raw).contains("Change Notion Access"));
        assert!(
            push_action_message("apply_failed", false, Some(raw)).contains("Change Notion Access")
        );
    }

    #[test]
    fn failed_journal_audit_alone_does_not_block_access_refresh() {
        let temp = TestTempDir::new("failed-journal-access-refresh");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");

        store
            .save_mount(MountConfig::new(mount_id.clone(), "notion", temp.path()))
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Page",
                    "page.md",
                )
                .with_hydration(HydrationState::Hydrated),
            )
            .expect("save entity");
        store
            .append_journal(JournalEntry::new(
                PushId("push-1".to_string()),
                mount_id.clone(),
                vec![remote_id.clone()],
                PushPlan::new(vec![remote_id], Vec::new()),
                JournalStatus::Failed("old failure".to_string()),
            ))
            .expect("append journal");

        assert!(
            !mount_has_pending_local_changes(&store, temp.path(), &mount_id)
                .expect("inspect pending")
        );
        assert!(
            !mount_has_unfinished_journals(&store, &mount_id).expect("inspect unfinished journals")
        );
    }

    #[test]
    fn unfinished_journal_blocks_access_cache_clear_until_push_finishes() {
        let temp = TestTempDir::new("unfinished-journal-access-refresh");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");

        store
            .save_mount(MountConfig::new(mount_id.clone(), "notion", temp.path()))
            .expect("save mount");
        store
            .append_journal(JournalEntry::new(
                PushId("push-1".to_string()),
                mount_id.clone(),
                vec![remote_id.clone()],
                PushPlan::new(vec![remote_id], Vec::new()),
                JournalStatus::Applying,
            ))
            .expect("append journal");

        assert!(
            mount_has_unfinished_journals(&store, &mount_id).expect("inspect unfinished journals")
        );
    }

    #[test]
    fn dirty_files_are_preserved_before_access_cache_clear() {
        let temp = TestTempDir::new("preserve-dirty-access-refresh");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let relative_path = Path::new("Team/Page/page.md");
        let frontmatter = "loc:\n  id: page-1\n  type: page\ntitle: Page\n";
        let body = "Original body.\n";
        let entity = EntityRecord::new(
            mount_id.clone(),
            remote_id.clone(),
            EntityKind::Page,
            "Page",
            relative_path,
        )
        .with_hydration(HydrationState::Dirty);

        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "notion", temp.path())
                    .projection(ProjectionMode::MacosFileProvider),
            )
            .expect("save mount");
        store.save_entity(entity).expect("save entity");
        store
            .save_shadow(
                &mount_id,
                ShadowDocument::from_synced_body(
                    remote_id,
                    body,
                    6,
                    vec![RemoteId::new("block-1")],
                )
                .expect("shadow")
                .with_frontmatter(frontmatter),
            )
            .expect("save shadow");

        let content_path =
            localityd::virtual_fs::virtual_fs_content_path(temp.path(), &mount_id, relative_path)
                .expect("content path");
        fs::create_dir_all(content_path.parent().expect("content parent"))
            .expect("create content parent");
        fs::write(&content_path, "local edit\n").expect("write local edit");

        let preserved = preserve_mount_pending_local_changes(&store, temp.path(), &mount_id)
            .expect("preserve")
            .expect("preserved changes");
        assert_eq!(preserved.count, 1);
        assert!(preserved.directory.join(relative_path).exists());

        clear_mount_cached_projection(&mut store, temp.path(), &mount_id).expect("clear cache");

        assert!(
            store
                .list_entities(&mount_id)
                .expect("list entities")
                .is_empty()
        );
        assert!(store.list_journal().expect("list journals").is_empty());
        assert!(!localityd::virtual_fs::virtual_fs_content_root(temp.path(), &mount_id).exists());
        assert!(preserved.directory.join(relative_path).exists());
    }

    #[derive(Clone)]
    struct FakeAccessRefreshSource {
        entries: Vec<TreeEntry>,
    }

    impl FakeAccessRefreshSource {
        fn new(entries: Vec<TreeEntry>) -> Self {
            Self { entries }
        }
    }

    impl locality_connector::Connector for FakeAccessRefreshSource {
        fn kind(&self) -> locality_connector::ConnectorKind {
            locality_connector::ConnectorKind("fake-access-refresh")
        }

        fn capabilities(&self) -> locality_connector::ConnectorCapabilities {
            locality_connector::ConnectorCapabilities::default()
        }

        fn supported_push_operations(&self) -> BTreeSet<locality_core::planner::PushOperationKind> {
            BTreeSet::new()
        }

        fn enumerate(
            &self,
            _request: locality_connector::EnumerateRequest,
        ) -> locality_core::LocalityResult<Vec<TreeEntry>> {
            Ok(self.entries.clone())
        }

        fn list_children(
            &self,
            request: locality_connector::ListChildrenRequest,
        ) -> locality_core::LocalityResult<locality_connector::ListChildrenResult> {
            match request.container {
                locality_connector::ChildContainer::Root => Ok(
                    locality_connector::ListChildrenResult::complete(self.entries.clone()),
                ),
                locality_connector::ChildContainer::PageChildren(_)
                | locality_connector::ChildContainer::DatabaseRows(_)
                | locality_connector::ChildContainer::DirectoryChildren(_) => {
                    Ok(locality_connector::ListChildrenResult::default())
                }
            }
        }

        fn fetch(
            &self,
            _request: locality_connector::FetchRequest,
        ) -> locality_core::LocalityResult<locality_connector::NativeEntity> {
            Err(locality_core::LocalityError::NotImplemented(
                "fake access refresh fetch",
            ))
        }

        fn render(
            &self,
            _entity: &locality_connector::NativeEntity,
        ) -> locality_core::LocalityResult<CanonicalDocument> {
            Err(locality_core::LocalityError::NotImplemented(
                "fake access refresh render",
            ))
        }

        fn parse(
            &self,
            _document: &CanonicalDocument,
        ) -> locality_core::LocalityResult<locality_connector::ParsedEntity> {
            Err(locality_core::LocalityError::NotImplemented(
                "fake access refresh parse",
            ))
        }

        fn check_concurrency(
            &self,
            _request: locality_connector::ApplyPlanRequest<'_>,
        ) -> locality_core::LocalityResult<()> {
            Err(locality_core::LocalityError::NotImplemented(
                "fake access refresh check concurrency",
            ))
        }

        fn apply(
            &self,
            _request: locality_connector::ApplyPlanRequest<'_>,
        ) -> locality_core::LocalityResult<locality_connector::ApplyPlanResult> {
            Err(locality_core::LocalityError::NotImplemented(
                "fake access refresh apply",
            ))
        }

        fn apply_undo(
            &self,
            _request: locality_connector::ApplyUndoRequest<'_>,
        ) -> locality_core::LocalityResult<locality_connector::ApplyUndoResult> {
            Err(locality_core::LocalityError::NotImplemented(
                "fake access refresh apply undo",
            ))
        }
    }

    impl localityd::hydration::HydrationSource for FakeAccessRefreshSource {
        fn fetch_render(
            &self,
            _request: &locality_core::hydration::HydrationRequest,
        ) -> locality_core::LocalityResult<localityd::hydration::HydratedEntity> {
            Err(locality_core::LocalityError::NotImplemented(
                "fake access refresh hydrate",
            ))
        }
    }

    impl localityd::source::SourcePushValidator for FakeAccessRefreshSource {}
    impl localityd::source::SourceAdapter for FakeAccessRefreshSource {}

    #[test]
    fn plain_file_remount_preserves_visible_dirty_file_before_clearing_state() {
        let temp = TestTempDir::new("plain-remount-preserves-visible-dirty-file");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-plain");
        let mount_root = temp.path().join("notion");
        let relative_path = Path::new("Team/Page/page.md");
        let visible_path = mount_root.join(relative_path);
        let visible_body = "---\ntitle: Page\n---\nplain visible edit\n";

        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "notion", &mount_root)
                    .projection(ProjectionMode::PlainFiles),
            )
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id.clone(),
                    remote_id,
                    EntityKind::Page,
                    "Page",
                    relative_path,
                )
                .with_hydration(HydrationState::Dirty),
            )
            .expect("save dirty entity");
        fs::create_dir_all(visible_path.parent().expect("visible parent"))
            .expect("create visible parent");
        fs::write(&visible_path, visible_body).expect("write visible dirty edit");

        let preserved =
            prepare_existing_workspace_mount_for_remount(&mut store, temp.path(), &mount_id)
                .expect("prepare remount")
                .expect("preserved plain-file dirty edit");

        let recovered = preserved.directory.join(relative_path);
        assert_eq!(preserved.count, 1);
        assert_eq!(
            fs::read_to_string(&recovered).expect("read recovered dirty edit"),
            visible_body
        );
        assert!(
            store
                .list_entities(&mount_id)
                .expect("list entities")
                .is_empty(),
            "stale state should still be cleared after preserving the visible edit"
        );
    }

    #[test]
    fn workspace_remount_preserves_pending_changes_and_clears_stale_cache() {
        let temp = TestTempDir::new("workspace-remount-clears-stale-cache");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let mount_root = temp.path().join("notion");
        let relative_path = Path::new("engineering-wiki/2026-06-26/page.md");
        let entity = EntityRecord::new(
            mount_id.clone(),
            remote_id.clone(),
            EntityKind::Page,
            "2026-06-26",
            relative_path,
        )
        .with_hydration(HydrationState::Dirty);

        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "notion", &mount_root)
                    .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");
        store.save_entity(entity).expect("save dirty entity");
        store
            .save_entity(EntityRecord::new(
                mount_id.clone(),
                RemoteId::new("old-folder"),
                EntityKind::Directory,
                "Old access folder",
                "engineering-wiki/old-access-folder",
            ))
            .expect("save stale directory entity");
        store
            .save_shadow(
                &mount_id,
                ShadowDocument::from_synced_body(
                    remote_id,
                    "remote body\n",
                    11,
                    vec![RemoteId::new("block-1")],
                )
                .expect("shadow"),
            )
            .expect("save shadow");
        let content_path =
            localityd::virtual_fs::virtual_fs_content_path(temp.path(), &mount_id, relative_path)
                .expect("content path");
        fs::create_dir_all(content_path.parent().expect("content parent"))
            .expect("create content parent");
        fs::write(&content_path, "pending local body\n").expect("write content cache");
        let visible_path = mount_root.join(relative_path);
        fs::create_dir_all(visible_path.parent().expect("visible parent"))
            .expect("create visible parent");
        fs::write(&visible_path, "stale visible body\n").expect("write visible stale file");
        let stale_empty_child = mount_root.join("engineering-wiki/old-access-folder");
        fs::create_dir_all(&stale_empty_child).expect("create stale visible folder");

        let preserved =
            prepare_existing_workspace_mount_for_remount(&mut store, temp.path(), &mount_id)
                .expect("prepare remount")
                .expect("preserved pending local change");

        assert_eq!(preserved.count, 1);
        assert!(preserved.directory.join(relative_path).exists());
        assert!(
            store.get_mount(&mount_id).expect("get mount").is_some(),
            "remount preparation must leave the mount record so run_mount can update it"
        );
        assert!(
            store
                .list_entities(&mount_id)
                .expect("list entities")
                .is_empty(),
            "stale page paths must not survive a workspace remount"
        );
        assert!(!localityd::virtual_fs::virtual_fs_content_root(temp.path(), &mount_id).exists());
        assert!(
            !visible_path.exists(),
            "stale materialized files from the previous access scope must not remain visible"
        );
        assert!(
            !stale_empty_child.exists(),
            "empty stale folders from the previous access scope must not remain visible"
        );

        let new_remote_id = RemoteId::new("new-page");
        let new_relative_path = Path::new("latest-access/page.md");
        let refreshed_mount = store
            .get_mount(&mount_id)
            .expect("get refreshed mount")
            .expect("mount remains after access refresh");
        let source = FakeAccessRefreshSource::new(vec![TreeEntry {
            mount_id: mount_id.clone(),
            remote_id: new_remote_id.clone(),
            kind: EntityKind::Page,
            title: "Latest access page".to_string(),
            path: new_relative_path.to_path_buf(),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: Some("2026-07-08T00:00:00.000Z".to_string()),
            stub_frontmatter: None,
        }]);

        let report = refresh_mount_root_after_access_change(
            &mut store,
            &source,
            temp.path(),
            &refreshed_mount,
        )
        .expect("populate refreshed access");

        assert_eq!(report.enumerated, 1);
        assert!(
            store
                .find_entity_by_path(&mount_id, new_relative_path)
                .expect("find latest entity")
                .is_some(),
            "access refresh should populate entities from the newly granted access"
        );
        let children = localityd::virtual_fs::virtual_fs_children(
            &store,
            &mount_id,
            &super::mount_point_identifier(&refreshed_mount),
        )
        .expect("list refreshed virtual children");
        assert!(
            children
                .children
                .iter()
                .any(|child| child.filename == "latest-access"),
            "virtual provider children should include the newly granted access"
        );
    }

    #[test]
    fn macos_file_provider_remount_does_not_delete_visible_placeholders_directly() {
        let temp = TestTempDir::new("workspace-remount-keeps-file-provider-placeholders");
        let mount_id = MountId::new("notion-main");
        let mount_root = temp.path().join("notion");
        let relative_path = Path::new("engineering-wiki/2026-06-26/page.md");
        let mount = MountConfig::new(mount_id.clone(), "notion", &mount_root)
            .projection(ProjectionMode::MacosFileProvider);
        let entity = EntityRecord::new(
            mount_id,
            RemoteId::new("page-1"),
            EntityKind::Page,
            "2026-06-26",
            relative_path,
        );
        let visible_path = mount_root.join(relative_path);
        fs::create_dir_all(visible_path.parent().expect("visible parent"))
            .expect("create visible parent");
        fs::write(&visible_path, "visible provider placeholder\n").expect("write visible");

        clear_visible_projection_paths(&mount, &[entity]).expect("clear visible projection paths");

        assert!(
            visible_path.exists(),
            "macOS File Provider placeholders should be refreshed through provider reimport, not deleted through the CloudStorage filesystem"
        );
    }

    #[test]
    fn reset_refreshes_visible_file_provider_copy_from_cache() {
        let temp = TestTempDir::new("reset-refreshes-visible-copy");
        let mount_id = MountId::new("notion-main");
        let mount_root = temp.path().join("notion");
        let relative_path = Path::new("engineering-wiki/standups/page.md");
        let mount = MountConfig::new(mount_id.clone(), "notion", &mount_root)
            .projection(ProjectionMode::MacosFileProvider);
        let content_path =
            localityd::virtual_fs::virtual_fs_content_path(temp.path(), &mount_id, relative_path)
                .expect("content path");
        let visible_path = mount_root.join(relative_path);
        fs::create_dir_all(content_path.parent().expect("content parent"))
            .expect("create content parent");
        fs::create_dir_all(visible_path.parent().expect("visible parent"))
            .expect("create visible parent");
        fs::write(&content_path, "remote restored body\n").expect("write cache");
        fs::write(&visible_path, "dirty visible body\n").expect("write visible");

        refresh_visible_target_from_cache(temp.path(), &mount, relative_path, &visible_path)
            .expect("refresh visible");

        assert_eq!(
            fs::read_to_string(&visible_path).expect("read visible"),
            "remote restored body\n"
        );
    }

    #[test]
    fn pending_changes_hide_clean_failed_journal_audit_entries() {
        let status = status_report_with_entry(status_entry(
            loc_cli::status::StatusState::Clean,
            loc_cli::status::StatusSyncState::ReviewNeeded,
            1,
            vec![
                status_issue("failed_journal", "1 push journal(s) failed"),
                status_issue("last_failure", "previous failure"),
            ],
        ));

        let store = InMemoryStateStore::new();
        assert!(pending_changes_from_status(&store, &status).is_empty());
    }

    #[test]
    fn pending_changes_do_not_lookup_live_mode_state_for_clean_entries() {
        let status = status_report_with_entry(status_entry(
            loc_cli::status::StatusState::Clean,
            loc_cli::status::StatusSyncState::AllSynced,
            0,
            vec![],
        ));

        assert!(pending_changes_from_status(&NoAutoSaveLookupStore, &status).is_empty());
    }

    #[test]
    fn pending_changes_keep_failed_journal_with_dirty_file() {
        let status = status_report_with_entry(status_entry(
            loc_cli::status::StatusState::Dirty,
            loc_cli::status::StatusSyncState::ReviewNeeded,
            1,
            vec![
                status_issue("failed_journal", "1 push journal(s) failed"),
                status_issue("last_failure", "previous failure"),
            ],
        ));

        let store = InMemoryStateStore::new();
        let changes = pending_changes_from_status(&store, &status);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].state, "blocked");
        assert!(changes[0].summary.contains("Previous push failed"));
    }

    #[test]
    fn pending_changes_describe_remote_deleted_entries() {
        let status = status_report_with_entry(status_entry(
            loc_cli::status::StatusState::Clean,
            loc_cli::status::StatusSyncState::RemoteUpdateAvailable,
            0,
            vec![status_issue("remote_deleted", "remote object was deleted")],
        ));

        let store = InMemoryStateStore::new();
        let changes = pending_changes_from_status(&store, &status);

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].state, "needs_review");
        assert_eq!(changes[0].summary, "remote page deleted or unavailable");
        assert_eq!(changes[0].issue_codes, vec!["remote_deleted"]);
    }

    #[test]
    fn pending_changes_repairs_clean_stale_remote_deleted_page() {
        let temp = TestTempDir::new("desktop-pending-repairs-clean-remote-delete");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            temp.path().join("notion"),
        )
        .projection(ProjectionMode::MacosFileProvider);
        let remote_id = RemoteId::new("page-1");
        let relative_path = PathBuf::from("Roadmap/page.md");
        let page_path = mount.root.join(&relative_path);
        let content_path =
            localityd::virtual_fs::virtual_fs_content_root(temp.path(), &mount.mount_id)
                .join(&relative_path);
        fs::create_dir_all(page_path.parent().expect("page parent")).expect("create page parent");
        fs::create_dir_all(content_path.parent().expect("content parent"))
            .expect("create content parent");
        let file_frontmatter = "loc:\n  id: page-1\n  type: page\n  synced_at: now\n  remote_edited_at: remote-v1\ntitle: Roadmap\n";
        let shadow_frontmatter = "loc:\n  id: page-1\n  type: page\ntitle: Roadmap\n";
        let body = "# Roadmap\n\nOriginal body.\n";
        let rendered = render_canonical_markdown(&CanonicalDocument::new(file_frontmatter, body));
        fs::write(&page_path, &rendered).expect("write clean visible page");
        fs::write(&content_path, rendered).expect("write clean content cache");

        let shadow = ShadowDocument::from_synced_body(
            remote_id.clone(),
            body,
            7,
            [RemoteId::new("heading-1"), RemoteId::new("paragraph-1")],
        )
        .expect("shadow")
        .with_frontmatter(shadow_frontmatter);
        store.save_mount(mount.clone()).expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount.mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    relative_path.clone(),
                )
                .with_hydration(HydrationState::Hydrated)
                .with_remote_edited_at("remote-v1"),
            )
            .expect("save entity");
        store
            .save_shadow(&mount.mount_id, shadow)
            .expect("save shadow");
        store
            .save_freshness_state(
                FreshnessStateRecord::new(
                    mount.mount_id.clone(),
                    remote_id.clone(),
                    FreshnessTier::Hot,
                )
                .remote_hint_pending(true),
            )
            .expect("save freshness");
        store
            .save_remote_observation(
                RemoteObservationRecord::new(
                    mount.mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "Roadmap",
                    relative_path.clone(),
                    "unix_ms:1",
                )
                .with_remote_version(RemoteVersion::new("remote-v2"))
                .deleted(true),
            )
            .expect("save observation");

        let changes = super::pending_changes_for_mount(&store, temp.path(), &mount.mount_id)
            .expect("pending changes");

        assert!(changes.is_empty());
        assert!(!page_path.exists());
        assert!(!content_path.exists());
        assert!(
            store
                .get_entity(&mount.mount_id, &remote_id)
                .expect("get entity")
                .is_none()
        );
    }

    #[test]
    fn desktop_pending_changes_ignore_other_mount_failed_dirty_entries() {
        let temp = TestTempDir::new("desktop-pending-selected-mount");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        let selected_mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            temp.path().join("notion"),
        );
        let other_mount = MountConfig::new(
            MountId::new("google-docs-main"),
            "google-docs",
            temp.path().join("google-docs"),
        );
        let remote_id = RemoteId::new("google-docs-page");

        store
            .save_mount(selected_mount.clone())
            .expect("save selected mount");
        store
            .save_mount(other_mount.clone())
            .expect("save other mount");
        store
            .save_entity(
                EntityRecord::new(
                    other_mount.mount_id.clone(),
                    remote_id.clone(),
                    EntityKind::Page,
                    "table-move-guard",
                    "table-move-guard/page.md",
                )
                .with_hydration(HydrationState::Dirty),
            )
            .expect("save dirty other entity");
        store
            .append_journal(JournalEntry::new(
                PushId("push-google-docs-failed".to_string()),
                other_mount.mount_id.clone(),
                vec![remote_id.clone()],
                PushPlan::new(vec![remote_id], Vec::new()),
                JournalStatus::Failed("google docs failed deletion".to_string()),
            ))
            .expect("append failed other journal");

        let changes =
            super::pending_changes_for_mount(&store, temp.path(), &selected_mount.mount_id)
                .expect("selected mount pending changes");

        let titles = changes
            .iter()
            .map(|change| change.title.as_str())
            .collect::<Vec<_>>();
        assert!(
            changes.is_empty(),
            "Live Mode pending changes for the selected mount must not include another mount: {titles:?}"
        );

        let other_changes =
            super::pending_changes_for_mount(&store, temp.path(), &other_mount.mount_id)
                .expect("other mount pending changes");
        assert_eq!(other_changes.len(), 1);
        assert_eq!(other_changes[0].title, "table-move-guard");
    }

    #[test]
    fn pending_changes_include_auto_save_state_for_enrolled_file() {
        let status = status_report_with_entry(status_entry(
            loc_cli::status::StatusState::Dirty,
            loc_cli::status::StatusSyncState::ReviewNeeded,
            0,
            vec![],
        ));
        let mut store = InMemoryStateStore::new();
        store
            .save_auto_save_enrollment(
                AutoSaveEnrollmentRecord::new(
                    MountId::new("notion-main"),
                    "page.md",
                    AutoSaveOrigin::LocalityCreated,
                    "now",
                )
                .blocked("deletions require review", "now"),
            )
            .expect("save enrollment");

        let changes = pending_changes_from_status(&store, &status);

        assert_eq!(changes.len(), 1);
        assert!(changes[0].live_mode.enabled);
        assert_eq!(changes[0].live_mode.state, "blocked");
        assert_eq!(changes[0].live_mode.label, "Live Mode blocked");
        assert_eq!(
            changes[0].live_mode.reason.as_deref(),
            Some("deletions require review")
        );
    }

    #[test]
    fn pull_report_message_explains_conflict_markers() {
        let report = loc_cli::pull::PullReport {
            ok: false,
            command: "pull".to_string(),
            via: "cli".to_string(),
            mount_id: "notion-main".to_string(),
            root: "/tmp/notion".to_string(),
            target: "/tmp/notion/page.md".to_string(),
            enumerated: 0,
            stubbed: 0,
            hydrated: 0,
            skipped_dirty: 1,
            conflicts: vec![localityd::pull::PullConflict {
                path: "page.md".to_string(),
                remote_id: "abc".to_string(),
            }],
        };

        assert!(pull_report_message(&report).contains("conflict markers"));
    }

    #[test]
    fn pull_report_message_explains_partial_mount_refresh_with_pending_changes() {
        let report = loc_cli::pull::PullReport {
            ok: false,
            command: "pull".to_string(),
            via: "cli".to_string(),
            mount_id: "notion-main".to_string(),
            root: "/tmp/notion".to_string(),
            target: "/tmp/notion".to_string(),
            enumerated: 1,
            stubbed: 2,
            hydrated: 0,
            skipped_dirty: 1,
            conflicts: vec![],
        };

        let message = pull_report_message(&report);

        assert!(message.contains("Synced available Notion updates"));
        assert!(message.contains("Reset to remote"));
    }

    #[test]
    fn reset_to_remote_message_confirms_discarded_local_edits() {
        let report = loc_cli::pull::PullReport {
            ok: true,
            command: "pull".to_string(),
            via: "cli".to_string(),
            mount_id: "notion-main".to_string(),
            root: "/tmp/notion".to_string(),
            target: "/tmp/notion/page.md".to_string(),
            enumerated: 0,
            stubbed: 0,
            hydrated: 1,
            skipped_dirty: 0,
            conflicts: vec![],
        };

        let message = reset_to_remote_message(&report);

        assert!(message.contains("Discarded local edits"));
        assert!(message.contains("latest Notion version"));
    }

    struct NoAutoSaveLookupStore;

    impl AutoSaveRepository for NoAutoSaveLookupStore {
        fn save_auto_save_enrollment(
            &mut self,
            _enrollment: AutoSaveEnrollmentRecord,
        ) -> StoreResult<()> {
            panic!("clean pending change scan should not save auto-save enrollments")
        }

        fn get_auto_save_enrollment(
            &self,
            _mount_id: &MountId,
            _path: &Path,
        ) -> StoreResult<Option<AutoSaveEnrollmentRecord>> {
            panic!("clean pending change scan should not query auto-save enrollments")
        }

        fn find_auto_save_enrollment_by_remote_id(
            &self,
            _mount_id: &MountId,
            _remote_id: &RemoteId,
        ) -> StoreResult<Option<AutoSaveEnrollmentRecord>> {
            panic!("clean pending change scan should not query auto-save enrollments by remote id")
        }

        fn list_auto_save_enrollments(
            &self,
            _mount_id: &MountId,
        ) -> StoreResult<Vec<AutoSaveEnrollmentRecord>> {
            panic!("clean pending change scan should not list auto-save enrollments")
        }

        fn delete_auto_save_enrollment(
            &mut self,
            _mount_id: &MountId,
            _path: &Path,
        ) -> StoreResult<()> {
            panic!("clean pending change scan should not delete auto-save enrollments")
        }
    }

    fn status_report_with_entry(
        entry: loc_cli::status::StatusEntry,
    ) -> loc_cli::status::StatusReport {
        loc_cli::status::StatusReport {
            ok: true,
            clean: false,
            command: "status",
            target: None,
            summary: loc_cli::status::StatusSummary::default(),
            mounts: vec![loc_cli::status::StatusMountReport {
                mount_id: "notion-main".to_string(),
                connector: "notion".to_string(),
                root: "/tmp/notion".to_string(),
                live_mode: loc_cli::status::StatusLiveMode {
                    enabled: false,
                    state: "off".to_string(),
                    label: "Live Mode off".to_string(),
                    reason: None,
                    last_run_at: None,
                },
                entries: vec![entry],
            }],
        }
    }

    fn status_entry(
        state: loc_cli::status::StatusState,
        sync_state: loc_cli::status::StatusSyncState,
        failed_journal_count: usize,
        issues: Vec<loc_cli::status::StatusIssue>,
    ) -> loc_cli::status::StatusEntry {
        loc_cli::status::StatusEntry {
            path: "page.md".to_string(),
            absolute_path: "/tmp/notion/page.md".to_string(),
            entity_id: "abc".to_string(),
            kind: "page".to_string(),
            title: "Page".to_string(),
            hydration: "hydrated".to_string(),
            state,
            sync_state,
            remote: loc_cli::status::StatusRemoteState::default(),
            issues,
            pending_journal_count: 0,
            failed_journal_count,
        }
    }

    fn status_issue(code: &str, message: &str) -> loc_cli::status::StatusIssue {
        loc_cli::status::StatusIssue {
            code: code.to_string(),
            message: message.to_string(),
        }
    }

    #[test]
    fn diff_report_message_handles_noop() {
        let report = loc_cli::diff::DiffReport {
            ok: true,
            command: "diff",
            path: "/tmp/notion/page.md".to_string(),
            mount_id: "notion-main".to_string(),
            entity_id: "abc".to_string(),
            validation: Vec::new(),
            plan: None,
            readable_diff: None,
            guardrail: loc_cli::diff::GuardrailOutput {
                decision: "proceed".to_string(),
                reasons: Vec::new(),
            },
            action: "noop".to_string(),
            unsupported: Vec::new(),
            message: None,
            suggested_fix: None,
            completed_stages: Vec::new(),
        };

        assert_eq!(
            diff_report_message(&report),
            "No local changes in this file."
        );
    }

    #[test]
    fn conflict_preview_extracts_marker_block() {
        let preview = conflict_preview(
            "before\nintro\n<<<<<<< LOCAL\nlocal body\n=======\nremote body\n>>>>>>> REMOTE\nafter\n",
        )
        .expect("preview");

        assert!(preview.contains("<<<<<<< LOCAL"));
        assert!(preview.contains("local body"));
        assert!(preview.contains("remote body"));
        assert!(preview.contains(">>>>>>> REMOTE"));
    }

    #[test]
    fn conflict_preview_accepts_spaced_marker_lines() {
        let preview = conflict_preview(
            "before\n  <<<<<<< ours\nlocal body\n  =======  \nremote body\n  >>>>>>> theirs\nafter\n",
        )
        .expect("preview");

        assert!(preview.contains("<<<<<<< ours"));
        assert!(preview.contains("local body"));
        assert!(preview.contains("remote body"));
        assert!(preview.contains(">>>>>>> theirs"));
    }

    #[test]
    fn editor_hydration_tracks_shadow_dirty_and_conflicted_content() {
        let temp = TestTempDir::new("editor-hydration");
        let mut store = SqliteStateStore::open(temp.path().to_path_buf()).expect("open store");
        let mount_id = MountId::new("notion-main");
        let remote_id = RemoteId::new("page-1");
        let frontmatter = "loc:\n  id: page-1\n  type: page\ntitle: Page\n";
        let body = "Original body.\n";
        let entity = EntityRecord::new(
            mount_id.clone(),
            remote_id.clone(),
            EntityKind::Page,
            "Page",
            "page.md",
        )
        .with_hydration(HydrationState::Hydrated);
        store
            .save_mount(MountConfig::new(mount_id.clone(), "notion", temp.path()))
            .expect("save mount");
        store.save_entity(entity.clone()).expect("save entity");
        store
            .save_shadow(
                &mount_id,
                ShadowDocument::from_synced_body(
                    remote_id.clone(),
                    body,
                    6,
                    vec![RemoteId::new("block-1")],
                )
                .expect("shadow")
                .with_frontmatter(frontmatter),
            )
            .expect("save shadow");
        let synced = render_canonical_markdown(&CanonicalDocument::new(frontmatter, body));

        assert_eq!(
            hydration_after_editor_write(&store, &entity, &synced),
            HydrationState::Hydrated
        );
        assert_eq!(
            hydration_after_editor_write(&store, &entity, "changed"),
            HydrationState::Dirty
        );
        assert_eq!(
            hydration_after_editor_write(
                &store,
                &entity,
                "<<<<<<< LOCAL\nlocal\n=======\nremote\n>>>>>>> REMOTE\n",
            ),
            HydrationState::Conflicted
        );
    }

    #[test]
    fn detects_stale_daemon_schema_reload_errors() {
        assert!(is_unsupported_schema_version_message(
            "reload_mounts_failed: io error: unsupported schema version 10; this binary supports up to 9"
        ));
        assert!(!is_unsupported_schema_version_message(
            "reload_mounts_failed: io error: database is locked"
        ));
    }

    #[test]
    fn locate_prioritizes_only_online_only_pages() {
        let online_page = search_result("page", "online_only");
        let hydrated_page = search_result("page", "ready");
        let online_database = search_result("database", "online_only");

        assert!(should_prioritize_located_result(&online_page));
        assert!(!should_prioritize_located_result(&hydrated_page));
        assert!(!should_prioritize_located_result(&online_database));
    }

    #[test]
    fn install_state_does_not_prompt_for_existing_sqlite_without_current_marker() {
        let temp = TestTempDir::new("install-state-existing-sqlite");
        fs::write(temp.path().join("state.sqlite3"), b"not a real sqlite db")
            .expect("write sqlite marker");

        let review = inspect_install_state(temp.path());

        assert!(!review.should_prompt);
        assert!(review.state_exists);
        assert!(review.sqlite_exists);
        assert_eq!(review.previous_build_id, None);
    }

    #[test]
    fn install_state_prompts_for_fresh_install_without_state() {
        let temp = TestTempDir::new("install-state-fresh");

        let review = inspect_install_state(temp.path());

        assert!(review.should_prompt);
        assert!(review.state_exists);
        assert!(!review.sqlite_exists);
        assert_eq!(review.previous_build_id, None);
    }

    #[test]
    fn install_state_marker_records_previous_build_without_prompting() {
        let temp = TestTempDir::new("install-state-current-marker");
        fs::write(temp.path().join("state.sqlite3"), b"not a real sqlite db")
            .expect("write sqlite marker");
        record_current_install_marker(temp.path()).expect("record marker");

        let review = inspect_install_state(temp.path());

        assert!(!review.should_prompt);
        assert!(review.previous_build_id.is_some());
        assert_eq!(
            review.previous_build_id.as_deref(),
            Some(review.current_build_id.as_str())
        );
    }

    #[test]
    fn acknowledge_install_state_refreshes_agent_guidance_once_without_mount() {
        let temp = TestTempDir::new("install-state-acknowledge-guidance");

        let first = acknowledge_install_state_at(temp.path()).expect("acknowledge install state");
        assert!(first.ok);
        assert!(first.message.contains("Locality install state recorded."));
        assert!(
            first
                .message
                .contains("Agent guidance will be prepared after a mount is created.")
        );
        assert!(
            !temp.path().join("state.sqlite3").exists(),
            "agent guidance refresh should not create a state database when no mount exists"
        );

        let second = acknowledge_install_state_at(temp.path()).expect("acknowledge install state");
        assert!(second.ok);
        assert_eq!(second.message, "Locality install state recorded.");
    }

    #[test]
    fn acknowledge_install_state_refreshes_agent_guidance_after_build_change() {
        let temp = TestTempDir::new("install-state-acknowledge-upgrade-guidance");
        record_current_install_marker(temp.path()).expect("record marker");
        let old_marker = serde_json::json!({
            "stateFormatVersion": DESKTOP_INSTALL_MARKER_VERSION,
            "appVersion": env!("CARGO_PKG_VERSION"),
            "appBuildId": "old-desktop-build",
            "daemonBuildId": current_daemon_build_id(),
        });
        fs::write(
            temp.path().join("desktop-install.json"),
            serde_json::to_string_pretty(&old_marker).expect("serialize old marker"),
        )
        .expect("write old marker");

        let report = acknowledge_install_state_at(temp.path()).expect("acknowledge install state");

        assert!(report.ok);
        assert!(
            report
                .message
                .contains("Agent guidance will be prepared after a mount is created.")
        );
    }

    #[test]
    fn install_state_does_not_prompt_for_legacy_marker_without_desktop_build_id() {
        let temp = TestTempDir::new("install-state-legacy-marker");
        fs::write(temp.path().join("state.sqlite3"), b"not a real sqlite db")
            .expect("write sqlite marker");
        let legacy_marker = serde_json::json!({
            "appVersion": env!("CARGO_PKG_VERSION"),
            "daemonBuildId": current_daemon_build_id(),
        });
        fs::write(
            temp.path().join("desktop-install.json"),
            serde_json::to_string_pretty(&legacy_marker).expect("serialize legacy marker"),
        )
        .expect("write legacy marker");

        let review = inspect_install_state(temp.path());

        assert!(!review.should_prompt);
        assert_eq!(
            review.previous_build_id.as_deref(),
            Some(current_daemon_build_id().as_str())
        );
    }

    #[test]
    fn install_state_does_not_prompt_when_desktop_build_changes() {
        let temp = TestTempDir::new("install-state-new-desktop-build");
        fs::write(temp.path().join("state.sqlite3"), b"not a real sqlite db")
            .expect("write sqlite marker");
        let old_marker = serde_json::json!({
            "stateFormatVersion": DESKTOP_INSTALL_MARKER_VERSION,
            "appVersion": env!("CARGO_PKG_VERSION"),
            "appBuildId": "old-desktop-build",
            "daemonBuildId": current_daemon_build_id(),
        });
        fs::write(
            temp.path().join("desktop-install.json"),
            serde_json::to_string_pretty(&old_marker).expect("serialize old marker"),
        )
        .expect("write old marker");

        let review = inspect_install_state(temp.path());

        assert!(!review.should_prompt);
        assert_eq!(
            review.previous_build_id.as_deref(),
            Some("old-desktop-build")
        );
        assert_eq!(review.current_build_id, current_desktop_build_id());
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cli_installer_creates_path_link() {
        let temp = TestTempDir::new("terminal-cli-link");
        let cli = temp.path().join("Locality.app/Contents/MacOS/loc");
        let link = temp.path().join("bin/loc");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::write(&cli, b"loc cli").expect("write cli");

        let installed = install_terminal_cli_link_at(&cli, &link).expect("install cli link");

        assert_eq!(installed, link);
        assert_eq!(
            terminal_cli_link_state(&link, &cli).expect("link state"),
            TerminalCliLinkState::Current
        );
        assert_eq!(fs::read_link(&link).expect("read cli link"), cli);
    }

    #[cfg(windows)]
    #[test]
    fn terminal_cli_installer_creates_windows_cmd_shim() {
        let temp = TestTempDir::new("terminal-cli-windows-shim");
        let cli = temp.path().join("app/loc.exe");
        let link = temp.path().join("bin/loc.cmd");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::write(&cli, b"loc cli").expect("write cli");

        let installed = install_terminal_cli_link_at(&cli, &link).expect("install cli shim");

        assert_eq!(installed, link);
        assert_eq!(
            terminal_cli_link_state(&link, &cli).expect("shim state"),
            TerminalCliLinkState::Current
        );
        let contents = fs::read_to_string(&link).expect("read shim");
        assert!(contents.contains(super::WINDOWS_TERMINAL_CLI_SHIM_MARKER));
        assert!(contents.contains(&cli.display().to_string()));
        assert!(contents.contains("%*"));
    }

    #[cfg(windows)]
    #[test]
    fn terminal_cli_installer_updates_stale_windows_cmd_shim() {
        let temp = TestTempDir::new("terminal-cli-windows-shim-refresh");
        let old_cli = temp.path().join("old/loc.exe");
        let new_cli = temp.path().join("new/loc.exe");
        let link = temp.path().join("bin/loc.cmd");
        fs::create_dir_all(old_cli.parent().expect("old cli parent")).expect("create old parent");
        fs::create_dir_all(new_cli.parent().expect("new cli parent")).expect("create new parent");
        fs::create_dir_all(link.parent().expect("link parent")).expect("create link parent");
        fs::write(&old_cli, b"old loc cli").expect("write old cli");
        fs::write(&new_cli, b"new loc cli").expect("write new cli");
        fs::write(&link, super::windows_terminal_cli_shim_contents(&old_cli))
            .expect("write stale shim");

        install_terminal_cli_link_at(&new_cli, &link).expect("refresh cli shim");

        let contents = fs::read_to_string(&link).expect("read shim");
        assert!(contents.contains(&new_cli.display().to_string()));
        assert!(!contents.contains(&old_cli.display().to_string()));
    }

    #[cfg(windows)]
    #[test]
    fn terminal_cli_installer_uses_cmd_file_on_windows() {
        let temp = TestTempDir::new("terminal-cli-windows-path-dir");
        let cli = temp.path().join("app/loc.exe");
        let writable = temp.path().join("writable-bin");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::create_dir_all(&writable).expect("create writable dir");
        fs::write(&cli, b"loc cli").expect("write cli");

        let installed = install_terminal_cli_link_in_path_dirs(&cli, vec![writable.clone()])
            .expect("install cli shim in path");

        assert_eq!(installed, writable.join("loc.cmd"));
        assert!(installed.is_file());
    }

    #[cfg(windows)]
    #[test]
    fn windows_terminal_lookup_checks_exe_and_cmd_extensions() {
        assert_eq!(
            super::command_path_candidates("loc"),
            vec!["loc.exe", "loc.cmd", "loc.bat", "loc"]
        );
        assert_eq!(super::terminal_cli_command_filename(), "loc.cmd");
    }

    #[cfg(windows)]
    #[test]
    fn windows_login_run_value_quotes_executable() {
        assert_eq!(
            super::windows_run_value_for_executable(Path::new(
                r"C:\Program Files\Locality\Locality.exe"
            )),
            r#""C:\Program Files\Locality\Locality.exe" --background"#
        );
    }

    #[test]
    fn desktop_background_launch_requires_explicit_argument() {
        assert!(!super::desktop_launch_requested_background_from_args([
            "Locality"
        ]));
        assert!(super::desktop_launch_requested_background_from_args([
            "Locality",
            "--background"
        ]));
        assert!(!super::desktop_launch_requested_background_from_args([
            "Locality",
            "--backgrounded"
        ]));
    }

    #[cfg(not(windows))]
    #[test]
    fn launch_agent_starts_desktop_in_background() {
        let plist = super::launch_agent_plist(Path::new(
            "/Applications/Locality.app/Contents/MacOS/Locality",
        ));

        assert!(plist.contains("<string>--background</string>"));
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cli_installer_updates_stale_symlink() {
        let temp = TestTempDir::new("terminal-cli-refresh");
        let old_cli = temp.path().join("old/loc");
        let new_cli = temp.path().join("new/loc");
        let link = temp.path().join("bin/loc");
        fs::create_dir_all(old_cli.parent().expect("old cli parent")).expect("create old parent");
        fs::create_dir_all(new_cli.parent().expect("new cli parent")).expect("create new parent");
        fs::create_dir_all(link.parent().expect("link parent")).expect("create link parent");
        fs::write(&old_cli, b"old loc cli").expect("write old cli");
        fs::write(&new_cli, b"new loc cli").expect("write new cli");
        std::os::unix::fs::symlink(&old_cli, &link).expect("create stale link");

        install_terminal_cli_link_at(&new_cli, &link).expect("refresh cli link");

        assert_eq!(fs::read_link(&link).expect("read cli link"), new_cli);
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cli_installer_does_not_replace_regular_file() {
        let temp = TestTempDir::new("terminal-cli-existing-file");
        let cli = temp.path().join("app/loc");
        let link = temp.path().join("bin/loc");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::create_dir_all(link.parent().expect("link parent")).expect("create link parent");
        fs::write(&cli, b"loc cli").expect("write cli");
        fs::write(&link, b"existing command").expect("write existing command");

        let error = install_terminal_cli_link_at(&cli, &link).expect_err("regular file rejected");

        assert!(error.contains("A file already exists"));
        assert_eq!(
            fs::read_to_string(&link).expect("read existing command"),
            "existing command"
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cli_installer_uses_later_path_directory_without_admin() {
        let temp = TestTempDir::new("terminal-cli-path-dirs");
        let cli = temp.path().join("app/loc");
        let occupied = temp.path().join("occupied-bin");
        let writable = temp.path().join("writable-bin");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::create_dir_all(&occupied).expect("create occupied dir");
        fs::create_dir_all(&writable).expect("create writable dir");
        fs::write(&cli, b"loc cli").expect("write cli");
        fs::write(occupied.join("loc"), b"existing command").expect("write occupied command");

        let installed =
            install_terminal_cli_link_in_path_dirs(&cli, vec![occupied.clone(), writable.clone()])
                .expect("install cli link in path");

        assert_eq!(installed, writable.join("loc"));
        assert_eq!(fs::read_link(&installed).expect("read cli link"), cli);
        assert_eq!(
            fs::read_to_string(occupied.join("loc")).expect("read occupied command"),
            "existing command"
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cli_installer_prefers_user_path_before_homebrew_fallback() {
        let temp = TestTempDir::new("terminal-cli-user-path");
        let cli = temp.path().join("app/loc");
        let homebrew = temp.path().join("opt/homebrew/bin");
        let user_bin = temp.path().join(".local/bin");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::create_dir_all(&homebrew).expect("create homebrew dir");
        fs::create_dir_all(&user_bin).expect("create user dir");
        fs::write(&cli, b"loc cli").expect("write cli");

        let installed =
            install_terminal_cli_link_in_path_dirs(&cli, vec![homebrew.clone(), user_bin.clone()])
                .expect("install cli link in user path");

        assert_eq!(installed, user_bin.join("loc"));
        assert_eq!(fs::read_link(&installed).expect("read cli link"), cli);
        assert!(!homebrew.join("loc").exists());
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cli_installer_uses_user_fallback_before_protected_system_paths() {
        let temp = TestTempDir::new("terminal-cli-user-fallback");
        let cli = temp.path().join("app/loc");
        let user_bin = temp.path().join(".local/bin");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::write(&cli, b"loc cli").expect("write cli");

        let mut dirs = vec![PathBuf::from("/usr/bin"), PathBuf::from("/sbin")];
        super::insert_user_terminal_cli_fallback_dir(&mut dirs, &user_bin);

        let installed = super::install_terminal_cli_link_in_sorted_path_dirs(&cli, dirs)
            .expect("install fallback");

        assert_eq!(installed, user_bin.join("loc"));
        assert_eq!(fs::read_link(&installed).expect("read cli link"), cli);
    }

    #[test]
    fn terminal_cli_shell_path_section_is_managed_and_idempotent() {
        let temp = TestTempDir::new("terminal-cli-shell-path");
        let profile = temp.path().join(".zprofile");
        let user_bin = temp.path().join(".local/bin");
        fs::write(&profile, "export EXISTING=1\n").expect("write profile");

        write_terminal_cli_path_section(&profile, &user_bin).expect("write path section");
        write_terminal_cli_path_section(&profile, &user_bin).expect("rewrite path section");

        let contents = fs::read_to_string(&profile).expect("read profile");
        assert!(contents.contains("export EXISTING=1"));
        assert_eq!(contents.matches("LOCALITY_TERMINAL_CLI_PATH").count(), 2);
        assert!(contents.contains("export PATH=\"$_loc_cli_dir:$PATH\""));
    }

    #[test]
    fn terminal_cli_shell_quote_escapes_single_quotes() {
        assert_eq!(shell_single_quote("/tmp/it's/loc"), "'/tmp/it'\"'\"'s/loc'");
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cli_link_removal_deletes_only_current_link() {
        let temp = TestTempDir::new("terminal-cli-remove-link");
        let cli = temp.path().join("Locality.app/Contents/MacOS/loc");
        let other_cli = temp.path().join("other/bin/loc");
        let link = temp.path().join("bin/loc");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::create_dir_all(other_cli.parent().expect("other parent")).expect("create other parent");
        fs::create_dir_all(link.parent().expect("link parent")).expect("create link parent");
        fs::write(&cli, b"loc").expect("write cli");
        fs::write(&other_cli, b"other").expect("write other cli");
        std::os::unix::fs::symlink(&cli, &link).expect("link cli");

        let removed = super::remove_terminal_cli_link_at(&cli, &link).expect("remove current link");

        assert!(removed);
        assert!(!link.exists());

        std::os::unix::fs::symlink(&other_cli, &link).expect("link other cli");
        let removed = super::remove_terminal_cli_link_at(&cli, &link).expect("preserve other link");

        assert!(!removed);
        assert_eq!(fs::read_link(&link).expect("read other link"), other_cli);
    }

    #[test]
    fn state_clear_removes_metadata_but_preserves_state_root() {
        let temp = TestTempDir::new("clear-state-root");
        let state_root = temp.path().join(".loc");
        fs::create_dir_all(state_root.join("content/notion-main")).expect("create content");
        fs::write(state_root.join("state.sqlite3"), b"db").expect("write db");

        clear_state_root_contents(&state_root).expect("clear state");

        assert!(state_root.exists());
        assert!(
            fs::read_dir(&state_root)
                .expect("read state root")
                .next()
                .is_none()
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn state_clear_preserves_windows_installed_app_files() {
        let temp = TestTempDir::new("clear-state-root-windows-install");
        let state_root = temp.path().join("Locality");
        fs::create_dir_all(state_root.join("content/notion-main")).expect("create content");
        fs::write(state_root.join("state.sqlite3"), b"db").expect("write db");
        fs::write(state_root.join("Locality.exe"), b"app").expect("write app exe");
        fs::write(state_root.join("locality-desktop.exe"), b"desktop").expect("write desktop exe");
        fs::write(state_root.join("localityd.exe"), b"daemon").expect("write daemon exe");
        fs::write(state_root.join("locality-cloud-files.exe"), b"provider")
            .expect("write provider exe");
        fs::write(state_root.join("loc.exe"), b"cli").expect("write cli exe");
        fs::write(state_root.join("uninstall.exe"), b"uninstall").expect("write uninstaller");
        fs::create_dir_all(state_root.join("bin")).expect("create bin");
        fs::write(state_root.join("bin/loc.cmd"), b"shim").expect("write terminal shim");

        clear_state_root_contents(&state_root).expect("clear state");

        assert!(!state_root.join("state.sqlite3").exists());
        assert!(!state_root.join("content").exists());
        assert_eq!(
            fs::read(state_root.join("Locality.exe")).expect("read app exe"),
            b"app"
        );
        assert_eq!(
            fs::read(state_root.join("locality-desktop.exe")).expect("read desktop exe"),
            b"desktop"
        );
        assert_eq!(
            fs::read(state_root.join("localityd.exe")).expect("read daemon exe"),
            b"daemon"
        );
        assert_eq!(
            fs::read(state_root.join("locality-cloud-files.exe")).expect("read provider exe"),
            b"provider"
        );
        assert_eq!(
            fs::read(state_root.join("loc.exe")).expect("read cli exe"),
            b"cli"
        );
        assert_eq!(
            fs::read(state_root.join("uninstall.exe")).expect("read uninstaller"),
            b"uninstall"
        );
        assert_eq!(
            fs::read(state_root.join("bin/loc.cmd")).expect("read terminal shim"),
            b"shim"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_reset_runtime_metadata_loads_provider_processes_without_sqlite_state() {
        let temp = TestTempDir::new("windows-reset-runtime-metadata");
        let state_root = temp.path().join("Locality");
        fs::create_dir_all(state_root.join("cloud-files-lifecycle")).expect("create lifecycle dir");
        fs::write(
            state_root.join("cloud-files-lifecycle/provider-a.json"),
            r#"{
  "root_id": "root-a",
  "pid": 1200,
  "helper": "C:\\Users\\vm-user\\AppData\\Local\\Locality\\locality-cloud-files.exe",
  "sync_root": "C:\\Users\\vm-user\\Locality",
  "state_dir": "C:\\Users\\vm-user\\AppData\\Local\\Locality",
  "stdout_log": "C:\\Users\\vm-user\\AppData\\Local\\Locality\\logs\\a.out.log",
  "stderr_log": "C:\\Users\\vm-user\\AppData\\Local\\Locality\\logs\\a.err.log"
}"#,
        )
        .expect("write lifecycle metadata");
        fs::write(
            state_root.join("cloud-files-lifecycle/invalid.json"),
            b"{not-json",
        )
        .expect("write invalid metadata");

        let runtimes =
            windows_cloud_files_runtime_processes_from_state(&state_root).expect("load runtimes");

        assert_eq!(
            runtimes,
            vec![WindowsCloudFilesRuntimeProcess {
                pid: 1200,
                helper: PathBuf::from(
                    r"C:\Users\vm-user\AppData\Local\Locality\locality-cloud-files.exe"
                ),
            }]
        );
    }

    #[test]
    fn state_watcher_ignores_sqlite_and_settings_churn() {
        let temp = TestTempDir::new("state-watch-ignore");
        let state_root = temp.path().join(".loc");
        let content_root = temp.path().join("group-content");
        let content_roots = vec![content_root];

        for path in [
            state_root.join("state.sqlite3"),
            state_root.join("state.sqlite3-wal"),
            state_root.join("state.sqlite3-shm"),
            state_root.join("desktop-activity.json"),
            state_root.join("desktop.json"),
            state_root.join("desktop-install.json"),
            state_root.join("logs/localityd.log"),
        ] {
            assert!(
                !state_event_path_requires_refresh(&path, &state_root, &content_roots),
                "{} should not refresh desktop surfaces",
                path.display()
            );
        }
    }

    #[test]
    fn state_watcher_uses_explicit_live_mode_signal_without_refreshing_on_sqlite() {
        let temp = TestTempDir::new("state-watch-live-mode");
        let state_root = temp.path().join(".loc");
        let content_roots = vec![temp.path().join("group-content")];

        for path in [
            state_root.join("state.sqlite3"),
            state_root.join("state.sqlite3-wal"),
            state_root.join("state.sqlite3-shm"),
        ] {
            assert!(
                !state_event_path_wakes_live_mode(&path, &state_root, &content_roots),
                "{} should not wake Live Mode",
                path.display()
            );
            assert!(
                !state_event_path_requires_refresh(&path, &state_root, &content_roots),
                "{} should not refresh desktop surfaces",
                path.display()
            );
        }

        assert!(state_event_path_wakes_live_mode(
            &state_root.join(LIVE_MODE_STATE_CHANGE_SIGNAL_FILE),
            &state_root,
            &content_roots
        ));
        assert!(state_event_path_requires_refresh(
            &state_root.join(LIVE_MODE_STATE_CHANGE_SIGNAL_FILE),
            &state_root,
            &content_roots
        ));
    }

    #[test]
    fn state_watcher_refreshes_for_user_visible_state_changes() {
        let temp = TestTempDir::new("state-watch-refresh");
        let state_root = temp.path().join(".loc");
        let content_root = temp.path().join("group-content");
        let content_roots = vec![content_root.clone()];

        assert!(!state_event_path_requires_refresh(
            &state_root.join("desktop-activity.json"),
            &state_root,
            &content_roots
        ));
        assert!(state_event_path_requires_refresh(
            &content_root.join("notion-main/files/Roadmap/page.md"),
            &state_root,
            &content_roots
        ));
        assert!(!state_event_path_wakes_live_mode(
            &content_root.join("notion-main/files/Roadmap/page.md"),
            &state_root,
            &content_roots
        ));
        assert!(!state_event_path_requires_refresh(
            &content_root.join("notion-main/files/Roadmap/.page.md.loc-tmp"),
            &state_root,
            &content_roots
        ));
    }

    #[test]
    fn live_mode_wake_signal_unblocks_wait_without_polling_state() {
        let mut generation = live_mode_wake_generation();

        wake_live_mode_runner();

        let started = Instant::now();
        assert!(wait_for_live_mode_state_change(
            &mut generation,
            Duration::from_secs(1)
        ));
        assert!(started.elapsed() < Duration::from_millis(100));

        let started = Instant::now();
        assert!(!wait_for_live_mode_state_change(
            &mut generation,
            Duration::from_millis(10)
        ));
        assert!(started.elapsed() >= Duration::from_millis(5));
    }

    #[test]
    fn live_mode_runner_ticks_immediately_then_waits_for_periodic_recheck() {
        let now = Instant::now();
        let mut next_tick = now;

        assert!(live_mode_runner_should_tick(false, now, &mut next_tick));
        assert!(!live_mode_runner_should_tick(
            false,
            now + LIVE_MODE_RUNNER_ACTIVE_INTERVAL,
            &mut next_tick
        ));
        assert!(live_mode_runner_should_tick(
            false,
            now + LIVE_MODE_RUNNER_PERIODIC_RECHECK,
            &mut next_tick
        ));
    }

    #[test]
    fn live_mode_runner_signal_ticks_before_periodic_recheck() {
        let now = Instant::now();
        let mut next_tick = now;

        assert!(live_mode_runner_should_tick(false, now, &mut next_tick));
        assert!(live_mode_runner_should_tick(
            true,
            now + LIVE_MODE_RUNNER_ACTIVE_INTERVAL,
            &mut next_tick
        ));
    }

    #[test]
    fn live_mode_remote_fast_forward_lease_suppresses_recent_duplicate() {
        let key = (
            MountId::new("lease-mount-duplicate"),
            RemoteId::new("lease-remote-duplicate"),
        );
        live_mode_release_remote_fast_forward_key(&key);
        let now = Instant::now();

        assert!(live_mode_claim_remote_fast_forward_key(&key, now));
        assert!(!live_mode_claim_remote_fast_forward_key(
            &key,
            now + Duration::from_millis(1)
        ));
        assert!(live_mode_claim_remote_fast_forward_key(
            &key,
            now + LIVE_MODE_REMOTE_FAST_FORWARD_LEASE + Duration::from_millis(1)
        ));

        live_mode_release_remote_fast_forward_key(&key);
    }

    #[test]
    fn live_mode_remote_fast_forward_lease_can_release_for_retry() {
        let key = (
            MountId::new("lease-mount-release"),
            RemoteId::new("lease-remote-release"),
        );
        live_mode_release_remote_fast_forward_key(&key);
        let now = Instant::now();

        assert!(live_mode_claim_remote_fast_forward_key(&key, now));
        live_mode_release_remote_fast_forward_key(&key);
        assert!(live_mode_claim_remote_fast_forward_key(
            &key,
            now + Duration::from_millis(1)
        ));

        live_mode_release_remote_fast_forward_key(&key);
    }

    #[test]
    fn desktop_activity_records_newest_first_and_caps_entries() {
        let temp = TestTempDir::new("desktop-activity");
        for index in 0..(DESKTOP_ACTIVITY_LIMIT + 2) {
            record_desktop_activity(
                temp.path(),
                &format!("Changed Notion access {index}"),
                "Connected Notion workspace. Locality refreshed the mounted folder.",
                "access",
            )
            .expect("record desktop activity");
        }

        let items = load_desktop_activity(temp.path()).expect("load desktop activity");

        assert_eq!(items.len(), DESKTOP_ACTIVITY_LIMIT);
        assert_eq!(
            items.first().map(|item| item.title.as_str()),
            Some("Changed Notion access 21")
        );
        assert_eq!(
            items.last().map(|item| item.title.as_str()),
            Some("Changed Notion access 2")
        );
        assert!(items.iter().all(|item| item.when == "Recent"));
        assert!(items.iter().all(|item| item.occurred_at.is_some()));
        assert!(items.iter().all(|item| {
            item.occurred_at
                .as_deref()
                .is_some_and(|value| value.starts_with("unix_ms:"))
        }));
    }

    struct TestTempDir {
        path: PathBuf,
    }

    impl TestTempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "locality-desktop-{label}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn test_connection(workspace_id: &str, workspace_name: &str) -> ConnectionRecord {
        ConnectionRecord {
            connection_id: ConnectionId::new("notion-default"),
            profile_id: None,
            connector: "notion".to_string(),
            display_name: "notion-default".to_string(),
            account_label: Some(workspace_name.to_string()),
            workspace_id: Some(workspace_id.to_string()),
            workspace_name: Some(workspace_name.to_string()),
            auth_kind: "oauth".to_string(),
            secret_ref: "connection:notion-default".to_string(),
            scopes: Vec::new(),
            capabilities_json: "{}".to_string(),
            status: "active".to_string(),
            created_at: "1".to_string(),
            updated_at: "1".to_string(),
            expires_at: None,
        }
    }

    fn search_result(kind: &str, state: &str) -> SearchResult {
        SearchResult {
            mount_id: "notion-main".to_string(),
            connector: "notion".to_string(),
            title: "Roadmap".to_string(),
            kind: kind.to_string(),
            remote_id: "page-1".to_string(),
            path: "Roadmap/page.md".to_string(),
            absolute_path: "/tmp/loc/Roadmap/page.md".to_string(),
            state: state.to_string(),
            safety: SearchSafety {
                agent_readable: state == "ready",
                labels: vec![state.to_string()],
            },
            remote: SearchRemoteState::default(),
            score: 0,
        }
    }

    fn tree_entry(remote_id: &str, title: &str, path: &str) -> TreeEntry {
        TreeEntry {
            mount_id: MountId::new("notion-main"),
            remote_id: RemoteId::new(remote_id),
            kind: EntityKind::Page,
            title: title.to_string(),
            path: PathBuf::from(path),
            hydration: HydrationState::Stub,
            content_hash: None,
            remote_edited_at: None,
            stub_frontmatter: None,
        }
    }
}

#[cfg(test)]
fn sample_snapshot() -> DesktopSnapshot {
    let mount = MountSummary {
        mount_id: "notion-main".to_string(),
        connector: "notion".to_string(),
        connector_name: "Notion".to_string(),
        connection_id: Some("codeflash".to_string()),
        workspace_name: "CodeFlash".to_string(),
        local_path: absolute_display_path(&default_notion_mount_root()),
        notion_url: Some("https://www.notion.so/37b3ac0ebb88802cbcf4d53c9cfc4972".to_string()),
        access_scope: "Initial Idea".to_string(),
        remote_root_id: Some("37b3ac0ebb88802cbcf4d53c9cfc4972".to_string()),
        projection: "macOS File Provider".to_string(),
        read_only: false,
        status: "ready".to_string(),
        root_exists: true,
        entity_count: 42,
        pending_change_count: 3,
        provider: None,
    };
    DesktopSnapshot {
        health: AppHealth {
            state: "ready".to_string(),
            attention_count: 3,
        },
        connection: ConnectionSummary {
            connector: "notion".to_string(),
            workspace_name: "CodeFlash".to_string(),
            account_label: "saurabh@codeflash.ai".to_string(),
            status: "ready".to_string(),
        },
        connections: vec![ConnectionSummary {
            connector: "notion".to_string(),
            workspace_name: "CodeFlash".to_string(),
            account_label: "saurabh@codeflash.ai".to_string(),
            status: "ready".to_string(),
        }],
        mount: mount.clone(),
        mounts: vec![mount],
        active_mount_id: Some("notion-main".to_string()),
        live_mode: MountLiveModeSummary::from_record(None, &sample_pending_changes()),
        needs_onboarding: false,
        settings: DesktopSettings {
            launch_at_login: true,
            show_menu_bar: true,
        },
        pending_changes: sample_pending_changes(),
        recent_files: vec![LocatedItem {
            title: "Roadmap 2026".to_string(),
            kind: "Page".to_string(),
            local_path: absolute_display_path(
                &default_notion_mount_root().join("Engineering/Roadmap 2026/page.md"),
            ),
            state: "ready".to_string(),
        }],
        activity: vec![
            ActivityItem {
                title: "Pushed Roadmap 2026 to Notion".to_string(),
                detail: "2 block edits".to_string(),
                when: "Today".to_string(),
                occurred_at: Some("unix_ms:1782033300000".to_string()),
                kind: "push".to_string(),
            },
            ActivityItem {
                title: "Located Launch Plan".to_string(),
                detail: "Prepared local path for an agent".to_string(),
                when: "Today".to_string(),
                occurred_at: Some("unix_ms:1782028800000".to_string()),
                kind: "locate".to_string(),
            },
            ActivityItem {
                title: "Connected Notion workspace CodeFlash".to_string(),
                detail: "Credentials stored in the OS credential store".to_string(),
                when: "Earlier".to_string(),
                occurred_at: Some("unix_ms:1781942400000".to_string()),
                kind: "connect".to_string(),
            },
        ],
        suggestions: vec![ConnectorSuggestion {
            connector: "Linear".to_string(),
            description: "Mount issues and projects as local files.".to_string(),
            state: "planned".to_string(),
        }],
    }
}

#[cfg(test)]
fn live_mode_target(path: &str) -> LiveModeRemoteTarget {
    LiveModeRemoteTarget {
        mount_id: MountId::new("notion-main"),
        remote_id: RemoteId::new("page-1"),
        path: PathBuf::from(path),
    }
}

#[cfg(test)]
struct LiveModeE2eContext {
    state_root: PathBuf,
    page_path: PathBuf,
    page_id: String,
    api: HttpNotionApi,
    scratch_page_id: Option<String>,
    temp_root: Option<PathBuf>,
    owns_daemon: bool,
}

#[cfg(test)]
impl Drop for LiveModeE2eContext {
    fn drop(&mut self) {
        if self.owns_daemon {
            let _ = run_daemon_control(&daemon_control_args_any_manager("stop", &self.state_root));
        }
        if let Some(page_id) = self.scratch_page_id.as_ref() {
            let _ = self.api.delete_block(page_id);
        }
        if let Some(temp_root) = self.temp_root.as_ref() {
            let _ = fs::remove_dir_all(temp_root);
        }
    }
}

#[cfg(test)]
struct LiveModeE2eCleanup {
    api: HttpNotionApi,
    page_id: String,
    run_id: String,
}

#[cfg(test)]
impl Drop for LiveModeE2eCleanup {
    fn drop(&mut self) {
        let _ = live_mode_e2e_delete_marker_blocks(&self.api, &self.page_id, &self.run_id);
    }
}

#[cfg(test)]
fn live_mode_e2e_context() -> LiveModeE2eContext {
    if let Ok(raw) = std::env::var("LOCALITY_DESKTOP_LIVE_MODE_E2E_PAGE")
        && !raw.trim().is_empty()
    {
        let page_path = live_mode_e2e_expand_path(&raw);
        return LiveModeE2eContext {
            state_root: default_state_root(),
            page_id: live_mode_e2e_page_id(&page_path),
            api: live_mode_e2e_notion_api(&page_path),
            page_path,
            scratch_page_id: None,
            temp_root: None,
            owns_daemon: false,
        };
    }
    live_mode_e2e_provision_scratch_context()
}

#[cfg(test)]
fn live_mode_e2e_expand_path(raw: &str) -> PathBuf {
    let expanded = expand_tilde(&raw).unwrap_or_else(|_| PathBuf::from(raw));
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().expect("current dir").join(expanded)
    }
}

#[cfg(test)]
fn live_mode_e2e_provision_scratch_context() -> LiveModeE2eContext {
    let token = live_mode_e2e_required_env("NOTION_TOKEN");
    let parent_page_id = live_mode_e2e_notion_page_id_from_raw(&live_mode_e2e_required_env(
        "LOCALITY_NOTION_LIVE_PARENT_PAGE",
    ));
    let api = HttpNotionApi::new(NotionConfig::default().with_token(token.clone()));
    let title = format!("Locality desktop Live Mode e2e {}", unique_suffix());
    let page = api
        .create_page(serde_json::json!({
            "parent": {
                "type": "page_id",
                "page_id": parent_page_id,
            },
            "properties": {
                "title": {
                    "title": [{
                        "type": "text",
                        "text": { "content": title }
                    }]
                }
            },
            "children": [{
                "object": "block",
                "type": "paragraph",
                "paragraph": {
                    "rich_text": [{
                        "type": "text",
                        "text": { "content": "Original paragraph for desktop Live Mode e2e." }
                    }]
                }
            }]
        }))
        .expect("create desktop Live Mode scratch page");
    let temp_root =
        std::env::temp_dir().join(format!("locality-desktop-live-e2e-{}", unique_suffix()));
    let state_root = temp_root.join(".loc");
    let mount_root = temp_root.join("Locality").join("notion");
    fs::create_dir_all(&state_root).expect("create live desktop state root");
    fs::create_dir_all(&mount_root).expect("create live desktop mount root");

    live_mode_e2e_seed_notion_connection(&state_root, &token);
    let mut store = SqliteStateStore::open(state_root.clone()).expect("open live desktop state");
    let mount_id = MountId::new("notion-main");
    run_mount(
        &mut store,
        MountOptions {
            mount_id: mount_id.clone(),
            connector: "notion".to_string(),
            root: mount_root.clone(),
            remote_root_id: Some(RemoteId::new(page.id.clone())),
            connection_id: Some(ConnectionId::new("notion-default")),
            read_only: false,
            projection: ProjectionMode::PlainFiles,
            settings_json: "{}".to_string(),
        },
    )
    .expect("mount live desktop scratch page");
    store
        .save_mount_live_mode(MountLiveModeRecord::new(
            mount_id,
            true,
            localityd::freshness::freshness_timestamp(),
        ))
        .expect("enable live mode for scratch mount");

    let connector = NotionConnector::new(NotionConfig::default().with_token(token));
    run_pull_with_state_root(
        &mut store,
        &connector,
        mount_root.clone(),
        Some(&state_root),
    )
    .expect("pull live desktop scratch page");
    start_live_mode_e2e_daemon(&state_root);
    let page_path = mount_root.join("page.md");
    assert!(
        page_path.exists(),
        "live desktop scratch pull should materialize {}",
        page_path.display()
    );

    LiveModeE2eContext {
        state_root,
        page_path,
        page_id: page.id.clone(),
        api,
        scratch_page_id: Some(page.id),
        temp_root: Some(temp_root),
        owns_daemon: true,
    }
}

#[cfg(test)]
fn live_mode_e2e_notion_page_id_from_raw(raw: &str) -> String {
    let compact = notion_id_from_url(raw).unwrap_or_else(|| raw.trim().replace('-', ""));
    if compact.len() == 32 && compact.chars().all(|ch| ch.is_ascii_hexdigit()) {
        format!(
            "{}-{}-{}-{}-{}",
            &compact[0..8],
            &compact[8..12],
            &compact[12..16],
            &compact[16..20],
            &compact[20..32]
        )
    } else {
        compact
    }
}

#[cfg(test)]
fn live_mode_e2e_required_env(key: &str) -> String {
    std::env::var(key)
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            panic!(
                "set {key} for live_mode_bidirectional_cloudstorage_markdown_e2e, or set LOCALITY_DESKTOP_LIVE_MODE_E2E_PAGE to an existing scratch page.md"
            )
        })
}

#[cfg(test)]
fn live_mode_e2e_seed_notion_connection(state_root: &Path, token: &str) {
    let now = localityd::freshness::freshness_timestamp();
    let connection_id = ConnectionId::new("notion-default");
    let profile_id = ConnectorProfileId::new(DEFAULT_NOTION_PROFILE_ID);
    let secret_ref = format!("connection:{}", connection_id.as_str());
    let credentials = open_credential_store(state_root);
    credentials
        .put(&secret_ref, token)
        .expect("seed live desktop Notion credential");
    let mut store = SqliteStateStore::open(state_root.to_path_buf()).expect("open live state");
    store
        .save_connector_profile(ConnectorProfileRecord {
            profile_id: profile_id.clone(),
            connector: "notion".to_string(),
            display_name: "Notion token auth".to_string(),
            auth_kind: "token".to_string(),
            scopes: vec![],
            capabilities_json: live_mode_e2e_notion_capabilities_json(),
            enabled_actions_json: "[\"read\",\"write\"]".to_string(),
            connector_version: "notion.v1".to_string(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now.clone(),
        })
        .expect("seed live desktop connector profile");
    store
        .save_connection(ConnectionRecord {
            connection_id,
            profile_id: Some(profile_id),
            connector: "notion".to_string(),
            display_name: "notion-default".to_string(),
            account_label: None,
            workspace_id: None,
            workspace_name: None,
            auth_kind: "token".to_string(),
            secret_ref,
            scopes: vec![],
            capabilities_json: live_mode_e2e_notion_capabilities_json(),
            status: "active".to_string(),
            created_at: now.clone(),
            updated_at: now,
            expires_at: None,
        })
        .expect("seed live desktop connection");
}

#[cfg(test)]
fn live_mode_e2e_notion_capabilities_json() -> String {
    serde_json::to_string(&ConnectorCapabilities {
        supports_block_updates: true,
        supports_databases: true,
        supports_oauth: true,
        supports_remote_observation: true,
        supports_lazy_child_enumeration: true,
        supports_media_download: true,
        supports_undo: true,
        supports_batch_observation: false,
    })
    .expect("serialize Notion capabilities")
}

#[cfg(test)]
fn start_live_mode_e2e_daemon(state_root: &Path) {
    let mut args = daemon_control_args("start", state_root);
    if !args.iter().any(|arg| arg == "--localityd-bin") {
        if let Some(localityd_bin) = live_mode_e2e_localityd_bin() {
            args.push("--localityd-bin".to_string());
            args.push(localityd_bin.display().to_string());
        }
    }
    let report = run_daemon_control(&args)
        .unwrap_or_else(|error| panic!("start live desktop daemon: {}", error.message()));
    assert_eq!(
        report.state,
        DaemonRunState::Running,
        "live desktop daemon should start: {report:?}"
    );
}

#[cfg(test)]
fn live_mode_e2e_localityd_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let debug_dir = exe.parent()?.parent()?;
    let candidate = debug_dir.join("localityd");
    candidate.exists().then_some(candidate)
}

#[cfg(test)]
fn live_mode_e2e_page_id(page_path: &Path) -> String {
    if let Ok(page_id) = std::env::var("LOCALITY_DESKTOP_LIVE_MODE_E2E_PAGE_ID")
        && !page_id.trim().is_empty()
    {
        return page_id.trim().to_string();
    }
    let contents = fs::read_to_string(page_path).expect("read live-mode page.md");
    locality_core::canonical::parse_canonical_markdown(&contents)
        .expect("parse Locality page.md frontmatter")
        .remote_id()
        .expect("page.md frontmatter must include loc.id")
        .0
        .clone()
}

#[cfg(test)]
fn live_mode_e2e_notion_api(page_path: &Path) -> HttpNotionApi {
    if let Ok(token) = std::env::var("NOTION_TOKEN")
        && !token.trim().is_empty()
    {
        return HttpNotionApi::new(NotionConfig::default().with_token(token));
    }

    let state_root = default_state_root();
    let store = SqliteStateStore::open(state_root.clone()).expect("open Locality state");
    let credentials = open_credential_store(&state_root);
    localityd::notion::resolve_notion_connector_for_path(&store, credentials.as_ref(), page_path)
        .expect("resolve Notion connector from desktop auth");
    let (mount, _) =
        resolve_desktop_mount_path(&store, page_path).expect("resolve live-mode mount path");
    let connection = mount
        .connection_id
        .as_ref()
        .and_then(|connection_id| {
            store
                .get_connection(connection_id)
                .expect("load mount connection")
        })
        .or_else(|| {
            store
                .list_connections()
                .expect("list connections")
                .into_iter()
                .find(|connection| {
                    connection.connector == "notion" && connection.status == "active"
                })
        })
        .expect("desktop Notion connection or NOTION_TOKEN");
    let secret = credentials
        .get(&connection.secret_ref)
        .expect("read Notion credential");
    let token = if connection.auth_kind == "oauth" {
        serde_json::from_str::<StoredNotionCredential>(&secret)
            .expect("decode stored Notion OAuth credential")
            .access_token
    } else {
        secret
    };

    HttpNotionApi::new(NotionConfig::default().with_token(token))
}

#[cfg(test)]
fn live_mode_e2e_append_local_marker(page_path: &Path, marker: &str) {
    let mut contents = fs::read_to_string(page_path).expect("read local page before edit");
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push('\n');
    contents.push_str(marker);
    contents.push('\n');
    fs::write(page_path, contents).expect("write local live-mode marker");
}

#[cfg(test)]
fn live_mode_e2e_append_remote_marker(api: &HttpNotionApi, page_id: &str, marker: &str) {
    api.append_block_children(
        page_id,
        serde_json::json!({
            "children": [{
                "object": "block",
                "type": "paragraph",
                "paragraph": {
                    "rich_text": [{
                        "type": "text",
                        "text": { "content": marker }
                    }]
                }
            }]
        }),
    )
    .expect("append remote Notion marker");
}

#[cfg(test)]
fn live_mode_e2e_wait_until<F>(label: &str, mut condition: F)
where
    F: FnMut() -> bool,
{
    let timeout = std::env::var("LOCALITY_DESKTOP_LIVE_MODE_E2E_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(90);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("{label} did not converge within {timeout}s");
}

#[cfg(test)]
fn live_mode_e2e_remote_text(api: &HttpNotionApi, page_id: &str) -> String {
    live_mode_e2e_remote_blocks(api, page_id)
        .iter()
        .map(live_mode_e2e_block_plain_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
fn live_mode_e2e_delete_marker_blocks(
    api: &HttpNotionApi,
    page_id: &str,
    run_id: &str,
) -> Result<(), String> {
    for block in live_mode_e2e_remote_blocks(api, page_id) {
        if live_mode_e2e_block_plain_text(&block).contains(run_id) {
            api.delete_block(&block.id)
                .map_err(|error| format!("delete marker block `{}`: {error}", block.id))?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn live_mode_e2e_remote_blocks(api: &HttpNotionApi, page_id: &str) -> Vec<BlockDto> {
    let mut blocks = Vec::new();
    let mut cursor = None;
    loop {
        let page = api
            .retrieve_block_children(page_id, cursor.as_deref())
            .expect("retrieve Notion page children");
        blocks.extend(page.results);
        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
    }
    blocks
}

#[cfg(test)]
fn live_mode_e2e_block_plain_text(block: &BlockDto) -> String {
    block
        .paragraph
        .as_ref()
        .map(live_mode_e2e_rich_text_block_plain_text)
        .unwrap_or_default()
}

#[cfg(test)]
fn live_mode_e2e_rich_text_block_plain_text(block: &RichTextBlockDto) -> String {
    block
        .rich_text
        .iter()
        .map(live_mode_e2e_rich_text_plain_text)
        .collect::<String>()
}

#[cfg(test)]
fn live_mode_e2e_rich_text_plain_text(text: &RichTextDto) -> String {
    if !text.plain_text.is_empty() {
        return text.plain_text.clone();
    }
    text.text
        .as_ref()
        .map(|text| text.content.clone())
        .unwrap_or_default()
}

#[cfg(test)]
fn unique_suffix() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    format!("{}-{millis}", std::process::id())
}

#[cfg(test)]
fn sample_pending_changes() -> Vec<PendingChange> {
    vec![
        PendingChange {
            mount_id: "notion-main".to_string(),
            entity_id: "roadmap-2026".to_string(),
            title: "Roadmap 2026".to_string(),
            local_path: "Engineering/Roadmap 2026/page.md".to_string(),
            summary: "2 text edits".to_string(),
            state: "safe".to_string(),
            issue_codes: Vec::new(),
            live_mode: sample_live_mode_status(false),
        },
        PendingChange {
            mount_id: "notion-main".to_string(),
            entity_id: "launch-plan".to_string(),
            title: "Launch Plan".to_string(),
            local_path: "Marketing/Launch Plan/page.md".to_string(),
            summary: "needs review: large deletion".to_string(),
            state: "needs_review".to_string(),
            issue_codes: vec!["large_deletion".to_string()],
            live_mode: sample_live_mode_status(false),
        },
        PendingChange {
            mount_id: "notion-main".to_string(),
            entity_id: "customer-notes".to_string(),
            title: "Customer Notes".to_string(),
            local_path: "Sales/Customer Notes/page.md".to_string(),
            summary: "1 property edit".to_string(),
            state: "safe".to_string(),
            issue_codes: Vec::new(),
            live_mode: sample_live_mode_status(true),
        },
    ]
}

#[cfg(test)]
fn sample_live_mode_status(enabled: bool) -> LiveModeFileStatus {
    LiveModeFileStatus {
        enabled,
        state: if enabled { "active" } else { "off" }.to_string(),
        label: if enabled {
            "Live Mode on"
        } else {
            "Live Mode off"
        }
        .to_string(),
        reason: None,
    }
}

#[cfg(windows)]
fn acquire_desktop_single_instance() -> Option<DesktopSingleInstanceGuard> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW};

    let mutex_name = windows_wide_null(WINDOWS_DESKTOP_SINGLE_INSTANCE_MUTEX);
    let mutex_handle = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr()) };
    if mutex_handle.is_null() {
        eprintln!("loc desktop could not create single-instance mutex");
        return Some(DesktopSingleInstanceGuard {
            mutex_handle,
            activation_event_handle: std::ptr::null_mut(),
        });
    }

    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_running {
        signal_existing_desktop_instance();
        unsafe {
            let _ = CloseHandle(mutex_handle);
        }
        return None;
    }

    let event_name = windows_wide_null(WINDOWS_DESKTOP_ACTIVATION_EVENT);
    let activation_event_handle =
        unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()) };
    if activation_event_handle.is_null() {
        eprintln!("loc desktop could not create single-instance activation event");
    }

    Some(DesktopSingleInstanceGuard {
        mutex_handle,
        activation_event_handle,
    })
}

#[cfg(not(windows))]
fn acquire_desktop_single_instance() -> Option<DesktopSingleInstanceGuard> {
    Some(DesktopSingleInstanceGuard {})
}

#[cfg(windows)]
fn signal_existing_desktop_instance() {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent};

    let event_name = windows_wide_null(WINDOWS_DESKTOP_ACTIVATION_EVENT);
    let event_handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr()) };
    if event_handle.is_null() {
        return;
    }

    unsafe {
        let _ = SetEvent(event_handle);
        let _ = CloseHandle(event_handle);
    }
}

fn start_desktop_activation_listener(app: AppHandle, activation_event_handle: Option<usize>) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

        let Some(activation_event_handle) = activation_event_handle else {
            return;
        };
        std::thread::spawn(move || {
            let activation_event_handle = activation_event_handle as HANDLE;
            loop {
                let result = unsafe { WaitForSingleObject(activation_event_handle, INFINITE) };
                if result != WAIT_OBJECT_0 {
                    break;
                }
                show_main_window_with_view(&app, None);
            }
        });
    }

    #[cfg(not(windows))]
    {
        let _ = app;
        let _ = activation_event_handle;
    }
}

#[cfg(windows)]
fn windows_wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() {
    if std::env::args().any(|arg| arg == "--prepare-uninstall") {
        let state_root = default_state_root();
        if let Err(error) = prepare_locality_uninstall_at(&state_root) {
            eprintln!("Locality uninstall preparation failed: {error}");
            std::process::exit(1);
        }
        return;
    }

    let background_launch = desktop_launch_requested_background();

    desktop_log("info", "app.start", "Locality desktop starting");
    let Some(single_instance_guard) = acquire_desktop_single_instance() else {
        return;
    };
    let activation_event_handle = single_instance_guard.activation_event_handle();
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init());
    let builder = if app_store_distribution() {
        builder
    } else {
        builder.plugin(tauri_plugin_updater::Builder::new().build())
    };

    builder
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && window.label() == "main"
            {
                api.prevent_close();
                let _ = window.hide();
                return;
            }
            if should_hide_tray_popover(window.label(), event) {
                let _ = window.hide();
            }
        })
        .setup(move |app| {
            if desktop_smoke_test_requested() {
                configure_main_window_chrome(app);
                // Keep release smoke tests isolated from the user's daemon state.
                std::process::exit(0);
            }
            if let Err(error) = apply_launch_at_login_preference() {
                eprintln!("loc desktop could not apply launch-at-login preference: {error}");
            }
            refresh_launch_at_login_cache_async();
            configure_main_window_chrome(app);
            build_tray(app)?;
            start_desktop_activation_listener(app.app_handle().clone(), activation_event_handle);
            sync_tray_visibility(app.app_handle(), &desktop_settings());
            start_state_change_watcher(app.app_handle().clone());
            ensure_runtime_ready_in_background(app.app_handle().clone());
            start_live_mode_runner(app.app_handle().clone());
            start_windows_cloud_files_provider_supervisor();
            if !background_launch {
                show_main_window_with_view(app.app_handle(), None);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            desktop_snapshot,
            debug_notion_queue_status,
            connect_notion,
            connect_notion_without_browser,
            change_notion_access,
            connect_google_docs,
            connect_gmail,
            notion_login_link,
            install_state_review,
            acknowledge_install_state,
            reset_locality_state,
            prepare_locality_uninstall,
            choose_mount_folder,
            ensure_runtime_ready,
            ensure_terminal_cli_available,
            create_workspace_mount,
            create_desktop_mount,
            connect_granola,
            reset_source_state,
            disconnect_source,
            export_source_backup,
            run_workspace_mount_onboarding,
            file_provider_enablement_status,
            reveal_file_provider_enablement,
            install_agent_guidance,
            locate_notion_page,
            search_notion_pages,
            review_push_plan,
            push_to_notion,
            push_notion_file,
            pull_notion_file,
            check_notion_file,
            keep_notion_file_as_draft,
            reset_notion_file_to_remote,
            live_mode_tick,
            set_mount_live_mode,
            diff_notion_file,
            inspect_notion_file,
            read_notion_file,
            save_notion_file,
            set_live_mode_for_file,
            set_auto_save_for_file,
            open_path,
            open_logs_folder,
            open_in_vs_code,
            open_mount_folder,
            open_mount_in_vs_code,
            reveal_path,
            show_main_window,
            set_desktop_setting,
            hide_menubar,
            quit_completely,
            schedule_update_relaunch,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Locality desktop app")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                show_main_window_with_view(app, None);
            }
        });
}

fn should_hide_tray_popover(window_label: &str, event: &tauri::WindowEvent) -> bool {
    window_label == "tray" && matches!(event, tauri::WindowEvent::Focused(false))
}

fn start_live_mode_runner(app: AppHandle) {
    std::thread::spawn(move || {
        let mut wake_generation = live_mode_wake_generation();
        let mut next_periodic_tick = Instant::now();
        loop {
            if !mount_live_mode_enabled() {
                wait_for_live_mode_state_change(
                    &mut wake_generation,
                    LIVE_MODE_RUNNER_IDLE_RECHECK,
                );
                next_periodic_tick = Instant::now();
                continue;
            }

            if live_mode_runner_should_tick(false, Instant::now(), &mut next_periodic_tick) {
                run_live_mode_tick_for_runner(&app);
                wake_generation = live_mode_wake_generation();
                continue;
            }

            let woke = wait_for_live_mode_state_change(
                &mut wake_generation,
                LIVE_MODE_RUNNER_ACTIVE_INTERVAL,
            );
            if !mount_live_mode_enabled() {
                continue;
            }
            if live_mode_runner_should_tick(woke, Instant::now(), &mut next_periodic_tick) {
                run_live_mode_tick_for_runner(&app);
            }
            wake_generation = live_mode_wake_generation();
        }
    });
}

fn live_mode_runner_should_tick(
    woke: bool,
    now: Instant,
    next_periodic_tick: &mut Instant,
) -> bool {
    if woke || now >= *next_periodic_tick {
        *next_periodic_tick = now + LIVE_MODE_RUNNER_PERIODIC_RECHECK;
        return true;
    }
    false
}

fn run_live_mode_tick_for_runner(app: &AppHandle) {
    let report = live_mode_tick_blocking();
    if live_mode_tick_should_refresh_surfaces(&report) {
        refresh_desktop_surfaces(app);
    }
}

fn mount_live_mode_enabled() -> bool {
    live_mode_enabled_mount(&default_state_root())
        .ok()
        .flatten()
        .is_some()
}

fn live_mode_tick_should_refresh_surfaces(report: &ActionReport) -> bool {
    !report.ok
        || report.message.contains("synced")
        || report.message.contains("pulled")
        || report.message.contains("paused")
        || report.message.contains("off")
}

fn configure_main_window_chrome(app: &mut tauri::App) {
    #[cfg(windows)]
    {
        if let Some(window) = app.get_webview_window("main") {
            if let Err(error) = window.set_decorations(false) {
                eprintln!("loc desktop could not configure Windows window chrome: {error}");
            }
        }
    }

    #[cfg(not(windows))]
    {
        let _ = app;
    }
}

#[cfg(windows)]
fn main_window_native_decorations() -> bool {
    false
}

#[cfg(not(windows))]
fn main_window_native_decorations() -> bool {
    true
}

fn build_main_window(app: &AppHandle) -> tauri::Result<()> {
    if app.get_webview_window("main").is_some() {
        return Ok(());
    }
    let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("Locality")
        .inner_size(960.0, 680.0)
        .min_inner_size(860.0, 620.0)
        .resizable(true)
        .center()
        .decorations(main_window_native_decorations())
        .transparent(false)
        .shadow(true)
        .visible(false);
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(TitleBarStyle::Overlay)
        .hidden_title(true);
    builder.build()?;
    Ok(())
}

fn refresh_agent_guidance_for_current_mount_at(
    state_root: &Path,
) -> Option<AgentGuidanceInstallReport> {
    let mount_path = agent_guidance_mount_path_at(state_root)?;
    let report = install_guidance_files(Some(&mount_path));
    if !report.ok {
        let failed = agent_guidance_failed_target_summary(&report);
        eprintln!("loc desktop could not refresh agent guidance: {failed}");
    }
    Some(report)
}

fn agent_guidance_failed_target_summary(report: &AgentGuidanceInstallReport) -> String {
    report
        .targets
        .iter()
        .filter(|target| target.status == "failed")
        .map(|target| target.detail.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

fn agent_guidance_mount_path_at(state_root: &Path) -> Option<String> {
    if !state_root.join("state.sqlite3").exists() {
        return None;
    }
    let store = SqliteStateStore::open(state_root.to_path_buf()).ok()?;
    let mounts = store.load_mounts().ok()?;
    let connections = store.list_connections().ok()?;
    let mount = choose_mount(&mounts, &connections)?;
    Some(display_path(&mount_access_root(&mount)))
}

fn build_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Locality", true, None::<&str>)?;
    let open_folder =
        MenuItem::with_id(app, "open_folder", "Open Notion Folder", true, None::<&str>)?;
    let review = MenuItem::with_id(
        app,
        "review_pending",
        "Review Pending Changes",
        true,
        None::<&str>,
    )?;
    let hide = MenuItem::with_id(
        app,
        "hide_menubar",
        "Don't Show in Menubar",
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(
        app,
        "quit_completely",
        "Quit Completely",
        true,
        None::<&str>,
    )?;
    let quit_options = Submenu::with_items(app, "Quit Options", true, &[&hide, &quit])?;
    let menu = Menu::with_items(app, &[&open, &open_folder, &review, &quit_options])?;
    let icon = tray_icon_image(TrayVisualState::Ready);

    TrayIconBuilder::with_id("main")
        .icon(icon)
        .icon_as_template(tray_icon_should_use_template())
        .tooltip("Locality")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                position,
                rect,
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_tray_popover(tray.app_handle(), tray_popover_anchor(position, rect));
            }
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main_window_with_view(app, None),
            "open_folder" => {
                if let Ok(snapshot) = load_desktop_snapshot() {
                    let path = expand_tilde(&snapshot.mount.local_path)
                        .unwrap_or_else(|_| PathBuf::from(snapshot.mount.local_path));
                    let _ = open_virtual_mount_or_path(&path);
                }
                show_main_window_with_view(app, Some("files"));
            }
            "review_pending" => show_main_window_with_view(app, Some("pending")),
            "hide_menubar" => {
                let _ = set_menu_bar_visible(app, false);
            }
            "quit_completely" => {
                stop_windows_cloud_files_provider_supervisor_for_shutdown();
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    refresh_tray_icon(app.app_handle());

    Ok(())
}

fn tray_popover_anchor(
    click_position: PhysicalPosition<f64>,
    icon_rect: Rect,
) -> PhysicalPosition<f64> {
    let rect_position = icon_rect.position.to_physical::<f64>(1.0);
    let rect_size = icon_rect.size.to_physical::<f64>(1.0);
    if rect_size.width <= 0.0 || rect_size.height <= 0.0 {
        return click_position;
    }

    PhysicalPosition::new(
        rect_position.x + (rect_size.width / 2.0),
        rect_position.y + rect_size.height,
    )
}

fn build_tray_popover(app: &AppHandle) -> tauri::Result<()> {
    if app.get_webview_window("tray").is_some() {
        return Ok(());
    }
    WebviewWindowBuilder::new(app, "tray", WebviewUrl::App("index.html#tray".into()))
        .title("Locality")
        .inner_size(TRAY_POPOVER_WIDTH, TRAY_POPOVER_HEIGHT)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .background_color(Color(0, 0, 0, 0))
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .visible(false)
        .build()?;

    Ok(())
}

fn toggle_tray_popover(app: &AppHandle, position: PhysicalPosition<f64>) {
    let window = match app.get_webview_window("tray") {
        Some(window) => window,
        None => {
            if let Err(error) = build_tray_popover(app) {
                desktop_log(
                    "warn",
                    "tray.popover_build_failed",
                    format!("could not build tray popover webview: {error}"),
                );
                show_main_window_with_view(app, None);
                return;
            }
            match app.get_webview_window("tray") {
                Some(window) => window,
                None => {
                    desktop_log(
                        "warn",
                        "tray.popover_missing_after_build",
                        "tray popover webview was not available after build",
                    );
                    show_main_window_with_view(app, None);
                    return;
                }
            }
        }
    };

    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        return;
    }

    let scale_factor = window.scale_factor().unwrap_or(1.0);
    let popover_size = window.outer_size().unwrap_or_else(|_| {
        PhysicalSize::new(
            (TRAY_POPOVER_WIDTH * scale_factor).round() as u32,
            (TRAY_POPOVER_HEIGHT * scale_factor).round() as u32,
        )
    });
    let screen_bounds = screen_bounds_for_tray_anchor(app, position);
    let popover_position = tray_popover_position(position, popover_size, screen_bounds);
    let _ = window.set_position(Position::Physical(popover_position));
    let _ = window.eval("window.dispatchEvent(new CustomEvent('loc-refresh-snapshot'));");
    let _ = window.show();
    let _ = window.set_focus();
    schedule_tray_icon_refresh(app.clone());
}

fn screen_bounds_for_tray_anchor(
    app: &AppHandle,
    position: PhysicalPosition<f64>,
) -> Option<ScreenBounds> {
    if let Ok(monitors) = app.available_monitors() {
        let monitor_bounds = monitors
            .iter()
            .map(monitor_screen_bounds)
            .collect::<Vec<_>>();
        if let Some(bounds) = screen_bounds_for_anchor_from_monitors(position, &monitor_bounds) {
            return Some(bounds);
        }
    }

    app.primary_monitor()
        .ok()
        .flatten()
        .map(|monitor| monitor_work_area_bounds(&monitor))
}

fn screen_bounds_for_anchor_from_monitors(
    anchor: PhysicalPosition<f64>,
    monitors: &[MonitorScreenBounds],
) -> Option<ScreenBounds> {
    monitors
        .iter()
        .find(|bounds| bounds.screen.contains(anchor))
        .or_else(|| {
            monitors.iter().min_by(|left, right| {
                left.screen
                    .distance_squared_to(anchor)
                    .total_cmp(&right.screen.distance_squared_to(anchor))
            })
        })
        .map(|bounds| bounds.work_area)
}

fn monitor_screen_bounds(monitor: &tauri::Monitor) -> MonitorScreenBounds {
    let position = monitor.position();
    let size = monitor.size();
    MonitorScreenBounds {
        screen: ScreenBounds {
            left: f64::from(position.x),
            top: f64::from(position.y),
            right: f64::from(position.x) + f64::from(size.width),
            bottom: f64::from(position.y) + f64::from(size.height),
        },
        work_area: monitor_work_area_bounds(monitor),
    }
}

fn monitor_work_area_bounds(monitor: &tauri::Monitor) -> ScreenBounds {
    let work_area = monitor.work_area();
    ScreenBounds {
        left: f64::from(work_area.position.x),
        top: f64::from(work_area.position.y),
        right: f64::from(work_area.position.x) + f64::from(work_area.size.width),
        bottom: f64::from(work_area.position.y) + f64::from(work_area.size.height),
    }
}

fn tray_popover_position(
    anchor: PhysicalPosition<f64>,
    popover_size: PhysicalSize<u32>,
    screen_bounds: Option<ScreenBounds>,
) -> PhysicalPosition<i32> {
    let width = f64::from(popover_size.width).max(TRAY_POPOVER_WIDTH);
    let height = f64::from(popover_size.height).max(TRAY_POPOVER_HEIGHT);
    let preferred_x = anchor.x - (width / 2.0);
    let preferred_y = anchor.y + TRAY_POPOVER_ANCHOR_OFFSET;

    let (x, y) = match screen_bounds {
        Some(bounds) => {
            let min_x = bounds.left + TRAY_POPOVER_EDGE_MARGIN;
            let max_x = bounds.right - width - TRAY_POPOVER_EDGE_MARGIN;
            let min_y = bounds.top + TRAY_POPOVER_EDGE_MARGIN;
            let max_y = bounds.bottom - height - TRAY_POPOVER_EDGE_MARGIN;
            let above_y = anchor.y - height - TRAY_POPOVER_ANCHOR_OFFSET;
            let y = if preferred_y > max_y && above_y >= min_y {
                above_y
            } else {
                clamp_axis(preferred_y, min_y, max_y)
            };

            (clamp_axis(preferred_x, min_x, max_x), y)
        }
        None => (
            preferred_x.max(TRAY_POPOVER_EDGE_MARGIN),
            preferred_y.max(TRAY_POPOVER_EDGE_MARGIN),
        ),
    };

    PhysicalPosition::new(x.round() as i32, y.round() as i32)
}

fn clamp_axis(value: f64, min: f64, max: f64) -> f64 {
    if max < min {
        min
    } else {
        value.clamp(min, max)
    }
}

fn show_main_window_with_view(app: &AppHandle, view: Option<&str>) {
    if let Some(popover) = app.get_webview_window("tray") {
        let _ = popover.hide();
    }

    let window = match app.get_webview_window("main") {
        Some(window) => window,
        None => {
            if let Err(error) = build_main_window(app) {
                desktop_log(
                    "warn",
                    "main_window.build_failed",
                    format!("could not build main window webview: {error}"),
                );
                return;
            }
            match app.get_webview_window("main") {
                Some(window) => window,
                None => {
                    desktop_log(
                        "warn",
                        "main_window.missing_after_build",
                        "main window webview was not available after build",
                    );
                    return;
                }
            }
        }
    };

    if let Some(view) = view {
        let escaped = view.replace('\\', "\\\\").replace('\'', "\\'");
        let _ = window.eval(format!(
            "window.dispatchEvent(new CustomEvent('loc-open-view', {{ detail: '{}' }}));",
            escaped
        ));
    }
    let _ = window.show();
    let _ = window.set_focus();
}
