#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(clippy::items_after_test_module)]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};
#[cfg(target_os = "macos")]
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use loc_cli::connect::{BrokerOAuthConnectOptions, run_connect_notion_broker_oauth};
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
use loc_cli::search::{
    SearchOptions, SearchResult, notion_id_from_url, run_search_with_access_roots,
};
use loc_cli::status::{StatusOptions, StatusState, StatusSyncState, run_status};
use locality_core::canonical::parse_canonical_markdown;
use locality_core::conflict::{
    has_unresolved_conflict_markers, render_inline_conflict_markdown_with_base,
};
use locality_core::freshness::RemoteVersion;
use locality_core::hydration::{HydrationReason, HydrationRequest};
use locality_core::journal::{JournalEntry, JournalStatus};
use locality_core::model::{EntityKind, HydrationState, MountId, RemoteId, TreeEntry};
#[cfg(test)]
use locality_notion::NotionConfig;
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
    append_service_log, bundled_binary_next_to_current_exe,
    default_state_root as platform_default_state_root, logs_dir as platform_logs_dir,
    user_home as platform_user_home,
};
use locality_store::{
    AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, AutoSaveState, ConnectionId,
    ConnectionRecord, ConnectionRepository, EntityRecord, EntityRepository,
    FreshnessStateRepository, HydrationJobRecord, HydrationJobRepository, JournalRepository,
    MountConfig, MountRepository, ProjectionMode, RemoteObservationRecord,
    RemoteObservationRepository, ShadowRepository, SqliteStateStore, VirtualMutationKind,
    VirtualMutationRecord, VirtualMutationRepository, open_credential_store,
};
use localityd::autosave::auto_save_timestamp;
use localityd::file_provider::{self as daemon_file_provider, ROOT_CONTAINER_IDENTIFIER};
use localityd::hydration::{HydrationExecutor, HydrationOutcome, HydrationSource};
use localityd::ipc::{DaemonBuildInfo, DaemonRequest, DaemonStatusReport, send_request};
use localityd::media::{document_with_absolute_media_hrefs, update_hydrated_media_manifest};
use localityd::push::execute_auto_save_push_job_with_content_root;
use localityd::source::{
    ResolvedSource, resolve_source_for_mount_id, resolve_source_for_path, source_display_name,
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
use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, Position, WebviewUrl, WebviewWindowBuilder,
    image::Image,
    menu::{Menu, MenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    webview::Color,
};
use tauri_plugin_dialog::DialogExt;

mod agent_guidance;

use agent_guidance::{
    AgentGuidanceInstallReport, install_agent_guidance as install_guidance_files,
};

#[cfg(any(not(windows), test))]
const TERMINAL_CLI_PATH_MANAGED_START: &str = "# >>> LOCALITY_TERMINAL_CLI_PATH >>>";
#[cfg(any(not(windows), test))]
const TERMINAL_CLI_PATH_MANAGED_END: &str = "# <<< LOCALITY_TERMINAL_CLI_PATH <<<";
#[cfg(windows)]
const WINDOWS_TERMINAL_CLI_SHIM_MARKER: &str = "LOCALITY_TERMINAL_CLI_SHIM";
#[cfg(windows)]
const WINDOWS_RUN_KEY_PATH: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const DEFAULT_NOTION_MOUNT_POINT_DIRECTORY: &str = "notion-main";
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

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopSnapshot {
    health: AppHealth,
    connection: ConnectionSummary,
    mount: MountSummary,
    mounts: Vec<MountSummary>,
    active_mount_id: Option<String>,
    needs_onboarding: bool,
    settings: DesktopSettings,
    pending_changes: Vec<PendingChange>,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityItem {
    title: String,
    detail: String,
    when: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    occurred_at: Option<String>,
    kind: String,
    undo_available: bool,
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

#[derive(Clone, Serialize, Deserialize)]
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

static CONNECT_NOTION_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static NOTION_LOGIN_LINK: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static SURFACE_REFRESH_STATE: OnceLock<Mutex<SurfaceRefreshState>> = OnceLock::new();
static LAUNCH_AT_LOGIN_STATE: OnceLock<Mutex<Option<bool>>> = OnceLock::new();
static LIVE_MODE_REMOTE_PULL_CURSOR: OnceLock<Mutex<usize>> = OnceLock::new();
static LIVE_MODE_LOCAL_RECONCILE_TIMES: OnceLock<Mutex<BTreeMap<PathBuf, Instant>>> =
    OnceLock::new();
#[cfg(target_os = "windows")]
static WINDOWS_CLOUD_FILES_PROVIDER_SUPERVISOR: OnceLock<
    Mutex<WindowsCloudFilesProviderSupervisor>,
> = OnceLock::new();
const DESKTOP_INSTALL_MARKER_VERSION: u32 = 2;
const DESKTOP_ACTIVITY_LIMIT: usize = 20;
const SURFACE_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(2);
const TRAY_POPOVER_WIDTH: f64 = 360.0;
const TRAY_POPOVER_HEIGHT: f64 = 520.0;
const TRAY_POPOVER_EDGE_MARGIN: f64 = 8.0;
const TRAY_POPOVER_ANCHOR_OFFSET: f64 = 12.0;
const LIVE_MODE_LOCAL_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

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

#[tauri::command]
async fn desktop_snapshot(app: AppHandle) -> DesktopSnapshot {
    let snapshot = match tauri::async_runtime::spawn_blocking(load_desktop_snapshot).await {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(message)) => degraded_snapshot(message),
        Err(error) => degraded_snapshot(format!("Could not load Locality desktop state: {error}")),
    };
    refresh_tray_icon_for_snapshot(&app, &snapshot);
    snapshot
}

#[tauri::command]
async fn connect_notion(app: AppHandle) -> ActionReport {
    run_notion_connection_flow(app, NotionConnectionAction::Connect).await
}

#[tauri::command]
async fn change_notion_access(app: AppHandle) -> ActionReport {
    run_notion_connection_flow(app, NotionConnectionAction::ChangeAccess).await
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
    let result =
        tauri::async_runtime::spawn_blocking(move || connect_notion_with_broker(state_root))
            .await
            .map_err(|error| format!("Notion OAuth worker failed: {error}"));
    CONNECT_NOTION_IN_PROGRESS.store(false, Ordering::Release);

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
        record_current_install_marker(&default_state_root())
    })
    .await
    .map_err(|error| format!("Install marker worker failed: {error}"))
    .and_then(|result| result)
    {
        Ok(()) => ActionReport {
            ok: true,
            message: "Locality install state recorded.".to_string(),
        },
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
        ensure_daemon_running(&state_root)
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
            return Err("Paste a Notion page URL or search your local Notion index.".to_string());
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
    let remote_target = match live_mode_next_remote_pull_target() {
        Ok(target) => target,
        Err(message) => {
            return ActionReport { ok: false, message };
        }
    };
    if let Some(target) = remote_target.as_ref()
        && live_mode_should_reconcile_local_target(target)
        && let Err(message) = live_mode_reconcile_local_target(target)
    {
        return ActionReport { ok: false, message };
    }

    match load_desktop_snapshot() {
        Ok(snapshot) => live_mode_tick_from_snapshot(
            &snapshot,
            remote_target.as_ref(),
            |_change, target| {
                let push_report = push_target_direct(target, false)?;
                if push_report_exit_code(&push_report) == 0 {
                    Ok(())
                } else {
                    Err(push_report_message(&push_report))
                }
            },
            live_mode_pull_remote_target_if_changed,
            live_mode_merge_remote_drift_target,
        ),
        Err(message) => ActionReport {
            ok: false,
            message: format!("Live Mode could not inspect the desktop state: {message}"),
        },
    }
}

fn live_mode_tick_from_snapshot<Sync, Pull, Merge>(
    snapshot: &DesktopSnapshot,
    remote_pull_target: Option<&LiveModeRemoteTarget>,
    mut sync_target: Sync,
    mut pull_remote_target: Pull,
    mut merge_remote_drift: Merge,
) -> ActionReport
where
    Sync: FnMut(&PendingChange, &Path) -> Result<(), String>,
    Pull: FnMut(&LiveModeRemoteTarget) -> Result<bool, String>,
    Merge: FnMut(&PendingChange, &Path) -> Result<bool, String>,
{
    if !live_mode_has_mounted_folder(snapshot) {
        return ActionReport {
            ok: false,
            message: "Live Mode needs a mounted Notion mount point.".to_string(),
        };
    }

    let Some(change) = snapshot.pending_changes.first() else {
        if let Some(target) = remote_pull_target {
            match pull_remote_target(target) {
                Ok(true) => {
                    return ActionReport {
                        ok: true,
                        message: "Live Mode pulled 1 remote change.".to_string(),
                    };
                }
                Ok(false) => {
                    return ActionReport {
                        ok: true,
                        message: "Live Mode checked 1 page for remote changes.".to_string(),
                    };
                }
                Err(message) => return ActionReport { ok: false, message },
            }
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
                Ok(true) => {
                    if let Err(message) = sync_target(change, &target) {
                        return ActionReport { ok: false, message };
                    }
                    return ActionReport {
                        ok: true,
                        message: "Live Mode merged remote updates and synced 1 pending change."
                            .to_string(),
                    };
                }
                Ok(false) => {}
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

fn live_mode_next_remote_pull_target() -> Result<Option<LiveModeRemoteTarget>, String> {
    let state_root = default_state_root();
    let store = SqliteStateStore::open(state_root)
        .map_err(|error| format!("Live Mode could not open Locality state: {error}"))?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Live Mode could not inspect mounted folders: {error}"))?;
    let Some(mount) = choose_mount(&mounts) else {
        return Ok(None);
    };

    live_mode_next_remote_pull_target_for_mount(&store, &mount)
}

fn live_mode_next_remote_pull_target_for_mount<S>(
    store: &S,
    mount: &MountConfig,
) -> Result<Option<LiveModeRemoteTarget>, String>
where
    S: EntityRepository,
{
    let candidates = live_mode_remote_pull_candidates(store, mount)?;
    if candidates.is_empty() {
        return Ok(None);
    }

    let mut cursor = live_mode_remote_pull_cursor()
        .lock()
        .map_err(|_| "Live Mode remote pull cursor is unavailable.".to_string())?;
    let index = *cursor % candidates.len();
    *cursor = cursor.wrapping_add(1);
    Ok(Some(candidates[index].clone()))
}

fn live_mode_remote_pull_cursor() -> &'static Mutex<usize> {
    LIVE_MODE_REMOTE_PULL_CURSOR.get_or_init(|| Mutex::new(0))
}

fn live_mode_remote_pull_candidates<S>(
    store: &S,
    mount: &MountConfig,
) -> Result<Vec<LiveModeRemoteTarget>, String>
where
    S: EntityRepository,
{
    let access_root = mount_access_root(mount);
    let mut candidates = store
        .list_entities(&mount.mount_id)
        .map_err(|error| format!("Live Mode could not inspect mounted pages: {error}"))?
        .into_iter()
        .filter(|entity| {
            entity.kind == EntityKind::Page && entity.hydration == HydrationState::Hydrated
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

fn live_mode_reconcile_local_target(target: &LiveModeRemoteTarget) -> Result<(), String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Live Mode could not open Locality state: {error}"))?;
    live_mode_reconcile_local_target_with_store(&mut store, &state_root, target)
}

fn live_mode_reconcile_local_target_with_store(
    store: &mut SqliteStateStore,
    state_root: &Path,
    target: &LiveModeRemoteTarget,
) -> Result<(), String> {
    daemon_file_provider::reconcile_newer_macos_file_provider_projection(
        store,
        state_root,
        Some(&target.path),
    )
    .map(|_| ())
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

fn live_mode_pull_remote_target_if_changed(target: &LiveModeRemoteTarget) -> Result<bool, String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Live Mode could not open Locality state: {error}"))?;
    let entity = store
        .get_entity(&target.mount_id, &target.remote_id)
        .map_err(|error| format!("Live Mode could not inspect mounted pages: {error}"))?
        .ok_or_else(|| {
            format!(
                "Live Mode could not find `{}` in the mounted folder.",
                target.remote_id.0
            )
        })?;
    let previous_shadow = live_mode_load_shadow(&store, target)?;

    let credentials = open_credential_store(&state_root);
    let connector = resolve_source_for_mount_id(&store, credentials.as_ref(), &target.mount_id)
        .map_err(|error| error.message())?;
    let render_request = HydrationRequest::new(
        target.mount_id.clone(),
        target.remote_id.clone(),
        entity.path.clone(),
        HydrationState::Hydrated,
        HydrationReason::RemoteFastForward,
    );
    let rendered = connector
        .fetch_render(&render_request)
        .map_err(|error| format!("Live Mode could not inspect Notion changes: {error}"))?;

    let remote_changed = previous_shadow
        .as_ref()
        .is_none_or(|shadow| shadow != &rendered.shadow)
        || live_mode_content_hash_changed(
            entity.content_hash.as_deref(),
            Some(rendered.shadow.body_hash.as_str()),
        );
    if !remote_changed {
        return Ok(false);
    }

    live_mode_reconcile_local_target_with_store(&mut store, &state_root, target)?;
    let entity = store
        .get_entity(&target.mount_id, &target.remote_id)
        .map_err(|error| format!("Live Mode could not inspect mounted pages: {error}"))?
        .ok_or_else(|| {
            format!(
                "Live Mode could not find `{}` in the mounted folder.",
                target.remote_id.0
            )
        })?;
    let previous_shadow = live_mode_load_shadow(&store, target)?;
    let content_path = virtual_fs_content_path(&state_root, &target.mount_id, &entity.path)
        .map_err(|error| format!("Live Mode could not resolve local content cache: {error}"))?;
    let output_root = virtual_fs_content_root(&state_root, &target.mount_id);
    let request = HydrationRequest::new(
        target.mount_id.clone(),
        target.remote_id.clone(),
        content_path,
        HydrationState::Hydrated,
        HydrationReason::RemoteFastForward,
    );
    let outcome = HydrationExecutor::new_with_output_root(&mut store, &connector, output_root)
        .hydrate_request(request)
        .map_err(|error| format!("Live Mode could not pull Notion changes: {error}"))?;
    if outcome == HydrationOutcome::SkippedDirty {
        return Ok(false);
    }

    if let Some(previous_shadow) = previous_shadow.as_ref() {
        daemon_file_provider::refresh_macos_file_provider_entity_projection_if_clean(
            &store,
            &state_root,
            &target.mount_id,
            &target.remote_id,
            previous_shadow,
        )
        .map_err(|error| {
            format!("Live Mode could not refresh the visible File Provider file: {error}")
        })?;
    }

    Ok(true)
}

fn live_mode_merge_remote_drift_target(
    _change: &PendingChange,
    target: &Path,
) -> Result<bool, String> {
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
        return Ok(false);
    };
    if entity.kind != EntityKind::Page {
        return Ok(false);
    }
    let previous_shadow = match store.load_shadow(&mount.mount_id, &entity.remote_id) {
        Ok(shadow) => shadow,
        Err(locality_store::StoreError::ShadowMissing { .. }) => return Ok(false),
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
        return Ok(false);
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
    let Some(merged) = live_mode_merge_remote_drift_markdown(
        &local_contents,
        previous_shadow.rendered_body.as_str(),
        &remote_document,
    ) else {
        return Ok(false);
    };

    write_file_atomic(&read_path, merged.as_bytes()).map_err(|error| {
        format!(
            "Live Mode could not write `{}`: {error}",
            read_path.display()
        )
    })?;
    if read_path != target && target.exists() {
        write_file_atomic(&target, merged.as_bytes()).map_err(|error| {
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
        entity.hydration = HydrationState::Dirty;
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

    Ok(true)
}

fn live_mode_merge_remote_drift_markdown(
    local_contents: &str,
    base_body: &str,
    remote_document: &locality_core::model::CanonicalDocument,
) -> Option<String> {
    let merged =
        render_inline_conflict_markdown_with_base(local_contents, Some(base_body), remote_document);
    if has_unresolved_conflict_markers(&merged) {
        return None;
    }
    Some(merged)
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

fn live_mode_load_shadow(
    store: &SqliteStateStore,
    target: &LiveModeRemoteTarget,
) -> Result<Option<locality_core::shadow::ShadowDocument>, String> {
    match store.load_shadow(&target.mount_id, &target.remote_id) {
        Ok(shadow) => Ok(Some(shadow)),
        Err(locality_store::StoreError::ShadowMissing { .. }) => Ok(None),
        Err(error) => Err(format!(
            "Live Mode could not inspect the current page shadow: {error}"
        )),
    }
}

fn live_mode_content_hash_changed(before: Option<&str>, after: Option<&str>) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => before != after,
        (None, None) => false,
        _ => true,
    }
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
    stop_windows_cloud_files_provider_supervisor();
    app.exit(0);
    ActionReport {
        ok: true,
        message: "Locality is quitting.".to_string(),
    }
}

fn load_desktop_snapshot() -> Result<DesktopSnapshot, String> {
    let state_root = default_state_root();
    let store = SqliteStateStore::open(state_root.clone()).map_err(|error| error.to_string())?;
    load_desktop_snapshot_from_store(&store, &state_root)
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
    let mount = choose_mount(&mounts);
    let connection = choose_connection(&connections, mount.as_ref());
    let needs_onboarding = desktop_needs_onboarding(connection.as_ref(), mount.as_ref());
    let mount_summaries = mounts
        .iter()
        .map(|mount| {
            let connection = choose_connection_for_mount(&connections, mount);
            let provider = provider_runtime_summary(state_root, mount);
            mount_summary(Some(store), Some(mount), connection.as_ref(), provider)
        })
        .collect::<Vec<_>>();
    let provider = mount
        .as_ref()
        .and_then(|mount| provider_runtime_summary(state_root, mount));
    let pending_changes = match mount.as_ref() {
        Some(mount) => pending_changes_for_mount(store, state_root, &mount.mount_id)?,
        None => Vec::new(),
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
        mount: mount_summary(Some(store), mount.as_ref(), connection.as_ref(), provider),
        mounts: mount_summaries,
        active_mount_id: mount.as_ref().map(|mount| mount.mount_id.0.clone()),
        needs_onboarding,
        settings: desktop_settings(),
        pending_changes,
        activity: activity_from_journals(&journals, store, state_root),
        suggestions: vec![ConnectorSuggestion {
            connector: "Linear".to_string(),
            description: "Mount issues and projects as local files.".to_string(),
            state: "planned".to_string(),
        }],
    })
}

fn degraded_snapshot(message: String) -> DesktopSnapshot {
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
        mounts: vec![mount_summary(None, None, None, None)],
        active_mount_id: None,
        needs_onboarding: false,
        settings: desktop_settings(),
        pending_changes: Vec::new(),
        activity: vec![ActivityItem {
            title: "Could not load Locality state".to_string(),
            detail: message,
            when: "Now".to_string(),
            occurred_at: Some(activity_timestamp()),
            kind: "error".to_string(),
            undo_available: false,
        }],
        suggestions: vec![ConnectorSuggestion {
            connector: "Linear".to_string(),
            description: "Mount issues and projects as local files.".to_string(),
            state: "planned".to_string(),
        }],
    }
}

fn choose_mount(mounts: &[MountConfig]) -> Option<MountConfig> {
    mounts
        .iter()
        .find(|mount| mount.connector == "notion")
        .or_else(|| mounts.first())
        .cloned()
}

fn desktop_needs_onboarding(
    connection: Option<&ConnectionRecord>,
    mount: Option<&MountConfig>,
) -> bool {
    !matches!(connection, Some(connection) if connection.status == "active") || mount.is_none()
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

fn mount_summary(
    store: Option<&SqliteStateStore>,
    mount: Option<&MountConfig>,
    connection: Option<&ConnectionRecord>,
    provider: Option<ProviderRuntimeSummary>,
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
        local_path: absolute_display_path(&mount_access_root(mount)),
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
        root_exists: mount_access_root(mount).exists(),
        entity_count: store
            .and_then(|store| store.list_entities(&mount.mount_id).ok())
            .map(|entities| entities.len())
            .unwrap_or(0),
        pending_change_count: store
            .map(|store| pending_changes_for_mount(store, &default_state_root(), &mount.mount_id))
            .and_then(Result::ok)
            .map(|changes| changes.len())
            .unwrap_or(0),
        provider,
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
            mount.entries.iter().map(move |entry| {
                (
                    MountId::new(mount.mount_id.clone()),
                    entry,
                    live_mode_status_for_entry(store, &MountId::new(mount.mount_id.clone()), entry),
                )
            })
        })
        .filter(|(_, entry, _)| status_entry_needs_desktop_attention(entry))
        .map(|(_, entry, live_mode)| PendingChange {
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
            let (detail, undo_available) = journal_detail(journal);
            ActivityItem {
                title,
                detail,
                when: "Recent".to_string(),
                occurred_at: journal_activity_timestamp(journal),
                kind: "push".to_string(),
                undo_available,
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
            undo_available: false,
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
            undo_available: false,
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

fn journal_detail(journal: &JournalEntry) -> (String, bool) {
    let operation_count = journal.plan.operations.len();
    match &journal.status {
        JournalStatus::Failed(message) => (message.clone(), false),
        JournalStatus::Prepared => (format!("{operation_count} changes prepared"), false),
        JournalStatus::Applying => (format!("{operation_count} changes applying"), false),
        JournalStatus::Applied | JournalStatus::Reconciled => {
            (format!("{operation_count} remote changes"), true)
        }
        JournalStatus::Reverted => (format!("{operation_count} changes reverted"), false),
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

        let _ = tray.set_icon_with_as_template(Some(tray_icon_image(state)), false);
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
    let paths = [
        ((8.6, 26.8), (5.8, 18.0)),
        ((5.8, 18.0), (8.6, 9.2)),
        ((27.4, 9.2), (30.2, 18.0)),
        ((30.2, 18.0), (27.4, 26.8)),
        ((11.8, 13.0), (24.2, 13.0)),
        ((11.8, 23.0), (24.2, 23.0)),
        ((15.1, 18.0), (20.9, 18.0)),
    ];

    for (start, end) in paths {
        draw_line(&mut rgba, size, start, end, 5.2, [255, 255, 255, 255]);
    }
    for (start, end) in paths {
        draw_line(&mut rgba, size, start, end, 3.4, [17, 24, 39, 255]);
    }

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

    Image::new_owned(rgba, size as u32, size as u32)
}

fn draw_line(
    rgba: &mut [u8],
    size: usize,
    start: (f64, f64),
    end: (f64, f64),
    width: f64,
    color: [u8; 4],
) {
    let half_width = width / 2.0;
    for y in 0..size {
        for x in 0..size {
            let px = x as f64 + 0.5;
            let py = y as f64 + 0.5;
            let distance = distance_to_segment((px, py), start, end);
            let alpha = (half_width + 0.7 - distance).clamp(0.0, 1.0);
            if alpha > 0.0 {
                blend_pixel(rgba, size, x, y, color, alpha);
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

fn distance_to_segment(point: (f64, f64), start: (f64, f64), end: (f64, f64)) -> f64 {
    let vx = end.0 - start.0;
    let vy = end.1 - start.1;
    let wx = point.0 - start.0;
    let wy = point.1 - start.1;
    let length_squared = vx * vx + vy * vy;
    if length_squared == 0.0 {
        return ((point.0 - start.0).powi(2) + (point.1 - start.1).powi(2)).sqrt();
    }

    let t = ((wx * vx + wy * vy) / length_squared).clamp(0.0, 1.0);
    let closest = (start.0 + t * vx, start.1 + t * vy);
    ((point.0 - closest.0).powi(2) + (point.1 - closest.1).powi(2)).sqrt()
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
    if notion_id_from_url(query).is_some() {
        prepare_exact_notion_url_path(query)?;
    }

    let results = search_notion_results(query, 1)?;
    let result = results.into_iter().next().ok_or_else(|| {
        if notion_id_from_url(query).is_some() {
            notion_access_miss_message()
        } else {
            "No local Notion page matched that search yet. Try a page title, path fragment, or Notion URL."
                .to_string()
        }
    })?;
    prioritize_located_notion_result(&result);
    Ok(located_item_for_search_result(result))
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
        return Err("Create a Notion folder before locating pages.".to_string());
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
        match connector.resolve_page_path_entries(mount.mount_id.clone(), &remote_id) {
            Ok(entries) if entries.iter().any(|entry| entry.remote_id == remote_id) => {
                save_exact_notion_entries(&mut store, entries)?;
                return Ok(());
            }
            Ok(_) => {
                last_error = Some(format!(
                    "Notion page `{}` was not returned while resolving its parent hierarchy.",
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
        return "That Notion page is outside the selected Notion access for this mount. Use Change Notion Access to select the page or teamspace, then sync the workspace.".to_string();
    };
    let mounts = store.load_mounts().unwrap_or_default();
    let connections = store.list_connections().unwrap_or_default();
    let mount = choose_mount(&mounts);
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
        "That Notion page is outside the selected Notion access for workspace `{workspace}`. Current mount access: `{access_scope}`.{root_hint} Use Change Notion Access to select this page or the correct teamspace, then sync the workspace."
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
        return Err("Create a Notion mount point before locating pages.".to_string());
    }

    Ok(run_search_with_access_roots(
        &store,
        SearchOptions {
            query: query.to_string(),
            connector: Some("notion".to_string()),
            limit,
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

    InstallStateReview {
        should_prompt: false,
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
    let secret_refs = connection_secret_refs(state_root);
    stop_daemon_for_reset(state_root);
    reset_platform_projection_state();
    remove_connection_secrets(state_root, secret_refs);
    remove_desktop_support_state()?;
    clear_state_root_contents(state_root)?;
    Ok(())
}

fn clear_state_root_contents(state_root: &Path) -> Result<(), String> {
    if !state_root.exists() {
        fs::create_dir_all(state_root)
            .map_err(|error| format!("Could not create Locality state folder: {error}"))?;
        return Ok(());
    }

    for entry in fs::read_dir(state_root)
        .map_err(|error| format!("Could not inspect Locality state folder: {error}"))?
    {
        let entry =
            entry.map_err(|error| format!("Could not inspect Locality state entry: {error}"))?;
        remove_path_if_exists(&entry.path())?;
    }
    Ok(())
}

fn connection_secret_refs(state_root: &Path) -> Vec<String> {
    let mut refs = vec![
        "connection:notion-default".to_string(),
        "connection:notion-main".to_string(),
        "connection:notion-test".to_string(),
    ];
    if state_root.join("state.sqlite3").exists()
        && let Ok(store) = SqliteStateStore::open(state_root.to_path_buf())
        && let Ok(connections) = store.list_connections()
    {
        refs.extend(
            connections
                .into_iter()
                .filter(|connection| !connection.secret_ref.is_empty())
                .map(|connection| connection.secret_ref),
        );
    }
    refs.sort();
    refs.dedup();
    refs
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

fn reset_platform_projection_state() {
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
    stop_windows_cloud_files_provider_supervisor();
}

fn remove_connection_secrets(state_root: &Path, secret_refs: Vec<String>) {
    let credentials = open_credential_store(state_root);
    for secret_ref in secret_refs {
        if let Err(error) = credentials.delete(&secret_ref) {
            desktop_log(
                "warn",
                "reset.credential_delete_failed",
                format!("could not delete credential `{secret_ref}`: {error}"),
            );
        }
    }
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
                    if !debounce_state_events_require_refresh(
                        &rx,
                        &state_root,
                        &content_roots,
                        event,
                    ) {
                        continue;
                    }
                    refresh_desktop_surfaces(&app);
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

fn debounce_state_events_require_refresh(
    rx: &mpsc::Receiver<notify::Result<notify::Event>>,
    state_root: &Path,
    content_roots: &[PathBuf],
    first_event: notify::Event,
) -> bool {
    let mut should_refresh = state_event_requires_refresh(&first_event, state_root, content_roots);
    std::thread::sleep(std::time::Duration::from_millis(150));
    while let Ok(event) = rx.try_recv() {
        match event {
            Ok(event) => {
                should_refresh |= state_event_requires_refresh(&event, state_root, content_roots);
            }
            Err(error) => eprintln!("loc desktop state watcher event failed: {error}"),
        }
    }
    should_refresh
}

fn state_event_requires_refresh(
    event: &notify::Event,
    state_root: &Path,
    content_roots: &[PathBuf],
) -> bool {
    event
        .paths
        .iter()
        .any(|path| state_event_path_requires_refresh(path, state_root, content_roots))
}

fn state_event_path_requires_refresh(
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
    if relative.as_os_str().is_empty() {
        return false;
    }

    let file_name = relative.file_name().and_then(|name| name.to_str());
    if file_name.is_some_and(|name| name.starts_with("state.sqlite3")) {
        return false;
    }

    match relative.components().next() {
        Some(std::path::Component::Normal(component)) => match component.to_str() {
            Some("content") => !is_virtual_content_temp_path(relative),
            Some("desktop-activity.json") => true,
            Some("credentials" | "desktop.json" | "desktop-install.json" | "logs") => false,
            _ => false,
        },
        _ => false,
    }
}

fn is_virtual_content_temp_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.') && name.ends_with(".loc-tmp"))
}

fn refresh_desktop_surfaces(app: &AppHandle) {
    schedule_desktop_surface_refresh(app.clone());
}

fn schedule_tray_icon_refresh(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let snapshot = tauri::async_runtime::spawn_blocking(load_desktop_snapshot)
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
    format!("\"{}\"", executable.display())
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
        if mount.projection == ProjectionMode::MacosFileProvider
            && let Ok(url) = macos_file_provider_domain_url(
                localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID,
            )
        {
            return url.join(mount_point_directory_name(mount));
        }
    }

    mount.root.clone()
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
            return Err("Choose a CloudStorage folder for the Notion mount.".to_string());
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
    validate_mount_root(root, state_root)?;

    #[cfg(target_os = "macos")]
    {
        if *projection == ProjectionMode::MacosFileProvider {
            let root = absolute_path(root)?;
            let provider_roots = macos_file_provider_cloud_storage_roots()
                .into_iter()
                .filter_map(|provider_root| absolute_path(&provider_root).ok())
                .collect::<Vec<_>>();
            let inside_provider_root = provider_roots
                .iter()
                .any(|provider_root| root.starts_with(provider_root) && root != *provider_root);
            if !inside_provider_root {
                return Err(format!(
                    "Choose a mount point inside the Locality File Provider root, for example {}.",
                    absolute_display_path(&default_notion_mount_root())
                ));
            }
        }
    }

    let _ = projection;
    Ok(())
}

fn validate_mount_root(root: &Path, state_root: &Path) -> Result<(), String> {
    if root.as_os_str().is_empty() {
        return Err("Choose a folder for the Notion mount.".to_string());
    }

    let root = absolute_path(root)?;
    let state_root = absolute_path(state_root)?;
    if root.starts_with(&state_root) {
        return Err("Choose a folder outside the Locality state directory.".to_string());
    }

    if let Ok(metadata) = fs::metadata(&root) {
        if !metadata.is_dir() {
            return Err(format!(
                "Choose a folder path, not a file: {}",
                root.display()
            ));
        }
        if metadata.permissions().readonly() {
            return Err(format!("Selected folder is read-only: {}", root.display()));
        }
        return Ok(());
    }

    let parent = root
        .ancestors()
        .skip(1)
        .find(|candidate| candidate.exists())
        .ok_or_else(|| format!("No existing parent folder for {}", root.display()))?;
    let metadata = fs::metadata(parent).map_err(|error| {
        format!(
            "Could not inspect parent folder `{}`: {error}",
            parent.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "Mount parent is not a folder: {}",
            parent.display()
        ));
    }
    if metadata.permissions().readonly() {
        return Err(format!(
            "Mount parent folder is read-only: {}",
            parent.display()
        ));
    }
    Ok(())
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
    let mut candidates = Vec::new();

    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from(
            "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
        ));
        if let Some(home) = platform_user_home() {
            candidates.push(
                home.join("Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"),
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
    let root = resolve_desktop_mount_root(&request.path)?;
    validate_desktop_mount_root(&root, &state_root, &projection)?;
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    if store
        .get_mount(&MountId::new(mount_id.clone()))
        .map_err(|error| format!("Could not inspect existing mounts: {error}"))?
        .is_some()
    {
        return Err(format!("Mount id `{mount_id}` already exists."));
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
            Some(RemoteId::new(workspace.to_string()))
        }
        other => {
            return Err(format!(
                "Desktop mount creation does not support connector `{other}`."
            ));
        }
    };

    let mount_report = run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new(mount_id),
            connector,
            root,
            remote_root_id,
            connection_id,
            read_only: request.read_only,
            projection: projection.clone(),
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

    Ok(format!(
        "Mounted {} at {} with {}.",
        connector_label(&mount.connector),
        absolute_display_path(&mount_access_root(&mount)),
        projection_label(&mount.projection)
    ))
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
    let current_build = expected_daemon_build_info();
    match running_daemon_build(state_root) {
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

    start_daemon_for_current_binary(state_root)
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

fn restart_daemon_for_current_binary(
    state_root: &Path,
    expected_build: &DaemonBuildInfo,
) -> Result<(), String> {
    let _ = run_daemon_control(&daemon_control_args_any_manager("stop", state_root));
    start_daemon_for_current_binary(state_root)?;
    match running_daemon_build(state_root) {
        Some(build) if &build == expected_build => Ok(()),
        Some(build) => Err(format!(
            "localityd restarted, but reported build {} instead of {}.",
            build.build_id, expected_build.build_id
        )),
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
    if wait_for_entities && mount.projection == ProjectionMode::MacosFileProvider {
        wait_for_virtual_projection_mount_point_children(state_root, mount)?;
    }
    register_virtual_projection(state_root, mount)?;
    prefetch_virtual_projection_root(state_root, mount)?;
    if wait_for_entities && mount.projection != ProjectionMode::MacosFileProvider {
        wait_for_mount_entities(state_root, &mount.mount_id)?;
    }
    ensure_virtual_projection_runtime(state_root, mount)?;
    signal_virtual_projection_refresh(mount);
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
            "waiting up to {}s for `{mount_id}:{mount_point_container_identifier}` to expose mount-point children before registering macOS File Provider",
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
            return Err(format!(
                "Notion connected, but Locality could not load any files for the mount point before mounting. Make sure at least one page is selected for Locality access, then try again. {diagnostic}"
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

fn stop_windows_cloud_files_provider_supervisor() {
    #[cfg(target_os = "windows")]
    {
        let Some(supervisor) = WINDOWS_CLOUD_FILES_PROVIDER_SUPERVISOR.get() else {
            return;
        };
        match supervisor.lock() {
            Ok(mut supervisor) => supervisor.stop_all(&default_state_root()),
            Err(_) => {
                eprintln!("loc desktop could not stop Windows Cloud Files providers: lock poisoned")
            }
        }
    }
}

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
            signal_macos_virtual_projection(&mount.mount_id.0, container_identifier)
        }
        ProjectionMode::LinuxFuse
        | ProjectionMode::PlainFiles
        | ProjectionMode::WindowsCloudFiles => Ok(()),
    }
}

#[cfg(target_os = "macos")]
fn signal_macos_virtual_projection(mount_id: &str, identifier: &str) -> Result<(), String> {
    let provider_identifier = macos_file_provider_item_identifier(mount_id, identifier);
    run_macos_file_provider_helper(
        "signal",
        vec![
            "--mount-id".to_string(),
            localityd::file_provider::MACOS_FILE_PROVIDER_DOMAIN_ID.to_string(),
            "--identifier".to_string(),
            provider_identifier,
        ],
    )
    .map(|_| ())
    .map_err(|error| {
        format!(
            "Could not signal macOS File Provider refresh for `{}`: {}",
            identifier,
            error.message()
        )
    })
}

#[cfg(target_os = "macos")]
fn macos_file_provider_item_identifier(mount_id: &str, identifier: &str) -> String {
    if identifier == ROOT_CONTAINER_IDENTIFIER {
        return ROOT_CONTAINER_IDENTIFIER.to_string();
    }
    format!(
        "m:{}:{}",
        macos_file_provider_encode_identifier_component(mount_id),
        macos_file_provider_encode_identifier_component(identifier)
    )
}

#[cfg(target_os = "macos")]
fn macos_file_provider_encode_identifier_component(value: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = value.as_bytes();
    let mut output = String::with_capacity((bytes.len() * 4).div_ceil(3));
    let mut index = 0;
    while index < bytes.len() {
        let first = bytes[index];
        let second = bytes.get(index + 1).copied();
        let third = bytes.get(index + 2).copied();

        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(
            TABLE[(((first & 0b0000_0011) << 4) | second.unwrap_or(0) >> 4) as usize] as char,
        );
        if let Some(second) = second {
            output.push(
                TABLE[(((second & 0b0000_1111) << 2) | third.unwrap_or(0) >> 6) as usize] as char,
            );
        }
        if let Some(third) = third {
            output.push(TABLE[(third & 0b0011_1111) as usize] as char);
        }

        index += 3;
    }
    output
}

#[cfg(not(target_os = "macos"))]
fn signal_macos_virtual_projection(_mount_id: &str, _identifier: &str) -> Result<(), String> {
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

fn connect_notion_with_broker(state_root: PathBuf) -> Result<String, String> {
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
        false,
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

fn connection_metadata_changed(
    previous: Option<&ConnectionRecord>,
    next: Option<&ConnectionRecord>,
) -> bool {
    previous.map(connection_metadata_key) != next.map(connection_metadata_key)
}

fn recoverable_macos_file_provider_activation_error(message: &str) -> bool {
    message.contains("The application cannot be used right now")
        || message.contains("locality-file-providerctl was not found")
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
        .map_err(|error| format!("Could not inspect cached Notion items: {error}"))?
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
    let pending_entities = store
        .list_entities(mount_id)
        .map_err(|error| format!("Could not inspect cached Notion items: {error}"))?
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
            state_root, mount_id, &directory, entity,
        )?);
    }
    for mutation in pending_mutations {
        items.push(preserve_virtual_mutation_local_change(
            state_root, mount_id, &directory, mutation,
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
    mount_id: &MountId,
    recovery_dir: &Path,
    entity: EntityRecord,
) -> Result<PreservedLocalChangeItem, String> {
    let source_path = virtual_fs_content_path(state_root, mount_id, &entity.path).ok();
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
    mount_id: &MountId,
    recovery_dir: &Path,
    mutation: VirtualMutationRecord,
) -> Result<PreservedLocalChangeItem, String> {
    let fallback_path =
        virtual_fs_content_path(state_root, mount_id, &mutation.projected_path).ok();
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
    store
        .clear_mount_source_state(mount_id)
        .map_err(|error| format!("Could not clear cached Notion mount state: {error}"))?;

    let content_root = virtual_fs_content_root(state_root, mount_id);
    if content_root.exists() {
        fs::remove_dir_all(&content_root).map_err(|error| {
            format!(
                "Could not clear cached Notion file contents at `{}`: {error}",
                content_root.display()
            )
        })?;
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
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    reconcile_desktop_projection_changes(&mut store, &state_root, Some(target))?;
    let credentials = open_credential_store(&state_root);
    let connector = resolve_source_for_path(&store, credentials.as_ref(), target)
        .map_err(|error| error.message())?;

    run_push_with_daemon_at_state_root(
        &mut store,
        &connector,
        target,
        PushOptions {
            assume_yes: true,
            confirm_dangerous,
        },
        Some(&state_root),
    )
    .map_err(|error| error.to_string())
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
    daemon_file_provider::reconcile_macos_file_provider_projection(store, state_root, target)
        .map(|_| ())
        .map_err(|error| format!("Could not reconcile macOS File Provider changes: {error}"))
}

fn auto_save_target_direct(target: &Path) -> Result<PushReport, String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open Locality state: {error}"))?;
    reconcile_desktop_projection_changes(&mut store, &state_root, Some(target))?;
    let credentials = open_credential_store(&state_root);
    let connector = resolve_source_for_path(&store, credentials.as_ref(), target)
        .map_err(|error| error.message())?;
    let report = execute_auto_save_push_job_with_content_root(
        &mut store,
        localityd::execution::PushJob {
            target_path: target.to_path_buf(),
            assume_yes: true,
            confirm_dangerous: false,
        },
        &connector,
        Some(&state_root),
    )
    .map_err(|error| error.to_string())?;
    Ok(PushReport::from_daemon(report))
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
    fs::write(&tmp, contents)?;
    fs::rename(tmp, path)
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
    if report.hydrated > 0 {
        return "Synced the latest Notion version for this file.".to_string();
    }
    if report.skipped_dirty > 0 {
        return "Locality kept your local edits because the file is still dirty. Review the diff, then push or restore the file.".to_string();
    }
    if report.enumerated > 0 || report.stubbed > 0 {
        return "Synced the latest Notion index for this mount.".to_string();
    }
    "Pulled the latest Notion content.".to_string()
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
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use loc_cli::search::{SearchRemoteState, SearchResult, SearchSafety};
    use locality_core::canonical::render_canonical_markdown;
    use locality_core::journal::{JournalEntry, JournalStatus, PushId};
    use locality_core::model::{
        CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId, TreeEntry,
    };
    use locality_core::planner::PushPlan;
    use locality_core::shadow::ShadowDocument;
    use locality_store::{
        AutoSaveEnrollmentRecord, AutoSaveOrigin, AutoSaveRepository, ConnectionId,
        ConnectionRecord, ConnectionRepository, ConnectorProfileId, EntityRecord, EntityRepository,
        InMemoryStateStore, JournalRepository, MountConfig, MountRepository, ProjectionMode,
        ShadowRepository, SqliteStateStore,
    };
    use localityd::ipc::DaemonBuildInfo;
    use tauri::{PhysicalPosition, PhysicalSize};

    use super::{
        DESKTOP_ACTIVITY_LIMIT, DESKTOP_INSTALL_MARKER_VERSION, LiveModeE2eCleanup, PendingChange,
        ScreenBounds, TerminalCliLinkState, TrayVisualState, clear_mount_cached_projection,
        clear_state_root_contents, conflict_preview, connection_metadata_changed,
        current_daemon_build_id, current_desktop_build_id, diff_report_message,
        exact_located_entity_record, failed_push_summary, has_unresolved_conflict_markers,
        hydration_after_editor_write, inspect_install_state, install_terminal_cli_link_at,
        install_terminal_cli_link_in_path_dirs, is_notion_access_lost_message,
        is_unsupported_schema_version_message, live_mode_content_hash_changed,
        live_mode_e2e_append_local_marker, live_mode_e2e_append_remote_marker,
        live_mode_e2e_notion_api, live_mode_e2e_page_id, live_mode_e2e_page_path,
        live_mode_e2e_remote_text, live_mode_e2e_wait_until, live_mode_merge_remote_drift_markdown,
        live_mode_remote_pull_candidates, live_mode_should_reconcile_local_target_for_key,
        live_mode_target, live_mode_tick_blocking, live_mode_tick_from_snapshot,
        load_desktop_activity, mount_has_pending_local_changes, mount_has_unfinished_journals,
        notion_id_from_url, parse_daemon_build_info_json, pending_changes_from_status,
        preserve_mount_pending_local_changes, pull_error_message, pull_report_message,
        push_action_message, record_current_install_marker, record_desktop_activity,
        sample_live_mode_status, sample_snapshot, shell_single_quote, should_hide_tray_popover,
        should_prioritize_located_result, state_event_path_requires_refresh,
        summarize_virtual_projection_children, terminal_cli_link_state, tray_icon_image,
        tray_popover_position, unique_suffix, validate_mount_root,
        virtual_projection_prefetch_container_identifiers,
        virtual_projection_refresh_signal_identifiers, write_terminal_cli_path_section,
    };

    #[test]
    fn desktop_state_root_absolutizes_relative_fallbacks() {
        assert!(super::absolute_state_root(PathBuf::from(".loc")).is_absolute());
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
        assert!(message.contains("select this page or the correct teamspace"));
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
    fn mount_summary_default_path_is_absolute() {
        let summary = super::mount_summary(None, None, None, None);

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

        let snapshot = super::load_desktop_snapshot_from_store(&store, temp.path())
            .expect("load snapshot from test store");

        assert_eq!(snapshot.mounts.len(), 2);
        assert_eq!(snapshot.active_mount_id.as_deref(), Some("notion-main"));
        assert_eq!(snapshot.mount.mount_id, "notion-main");
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
        let mut pull_calls = 0usize;

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            None,
            |_, _| {
                sync_calls += 1;
                Ok(())
            },
            |_| {
                pull_calls += 1;
                Ok(false)
            },
            |_, _| panic!("no pending changes should not merge remote drift"),
        );

        assert!(report.ok);
        assert_eq!(report.message, "Live Mode checked for changes.");
        assert_eq!(sync_calls, 0);
        assert_eq!(pull_calls, 0);
    }

    #[test]
    fn live_mode_tick_pulls_remote_candidate_without_pending_changes() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes.clear();
        let target = live_mode_target("/tmp/Locality/notion/teamspace-home/hello-world/page.md");
        let mut sync_calls = 0usize;
        let mut pulled = Vec::new();

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            Some(&target),
            |_, _| {
                sync_calls += 1;
                Ok(())
            },
            |target| {
                pulled.push(target.path.clone());
                Ok(false)
            },
            |_, _| panic!("remote-only tick should not merge local drift"),
        );

        assert!(report.ok);
        assert_eq!(
            report.message,
            "Live Mode checked 1 page for remote changes."
        );
        assert_eq!(sync_calls, 0);
        assert_eq!(pulled, vec![target.path]);
    }

    #[test]
    fn live_mode_tick_reports_remote_pull_when_file_changed() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes.clear();
        let target = live_mode_target("/tmp/Locality/notion/teamspace-home/hello-world/page.md");

        let report = live_mode_tick_from_snapshot(
            &snapshot,
            Some(&target),
            |_, _| panic!("remote pull should not sync local changes"),
            |_| Ok(true),
            |_, _| panic!("remote-only tick should not merge local drift"),
        );

        assert!(report.ok);
        assert_eq!(report.message, "Live Mode pulled 1 remote change.");
    }

    #[test]
    fn live_mode_content_hash_changed_detects_remote_body_change() {
        assert!(live_mode_content_hash_changed(Some("old"), Some("new")));
        assert!(!live_mode_content_hash_changed(Some("same"), Some("same")));
        assert!(live_mode_content_hash_changed(None, Some("hydrated")));
        assert!(!live_mode_content_hash_changed(None, None));
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
        )
        .expect("non-overlapping changes merge cleanly");

        assert!(!has_unresolved_conflict_markers(&merged));
        assert!(merged.contains("Remote intro."));
        assert!(merged.contains("Local middle."));
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

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires LOCALITY_DESKTOP_LIVE_MODE_E2E=1, a scratch CloudStorage page path, and live Notion auth; mutates then cleans up the page"]
    fn live_mode_bidirectional_cloudstorage_markdown_e2e() {
        assert_eq!(
            std::env::var("LOCALITY_DESKTOP_LIVE_MODE_E2E").as_deref(),
            Ok("1"),
            "set LOCALITY_DESKTOP_LIVE_MODE_E2E=1 to confirm this live destructive test"
        );
        let page_path = live_mode_e2e_page_path();
        assert!(
            page_path
                .to_string_lossy()
                .contains("/Library/CloudStorage/"),
            "LOCALITY_DESKTOP_LIVE_MODE_E2E_PAGE must be a CloudStorage-visible page.md path"
        );
        let page_id = live_mode_e2e_page_id(&page_path);
        let api = live_mode_e2e_notion_api(&page_path);
        let run_id = format!("locality-desktop-live-e2e-{}", unique_suffix());
        let cleanup = LiveModeE2eCleanup {
            api: api.clone(),
            page_id: page_id.clone(),
            run_id: run_id.clone(),
        };

        live_mode_e2e_wait_until("initial page is clean", || {
            live_mode_tick_blocking().ok
                && fs::read_to_string(&page_path)
                    .map(|contents| !contents.contains("<<<<<<<"))
                    .unwrap_or(false)
        });

        for index in 1..=3 {
            let marker = format!("{run_id} local-to-cloud {index}");
            live_mode_e2e_append_local_marker(&page_path, &marker);
            live_mode_e2e_wait_until(&format!("local marker {index} reaches Notion"), || {
                live_mode_tick_blocking().ok
                    && live_mode_e2e_remote_text(&api, &page_id).contains(&marker)
            });
        }

        for index in 1..=3 {
            let marker = format!("{run_id} cloud-to-local {index}");
            live_mode_e2e_append_remote_marker(&api, &page_id, &marker);
            live_mode_e2e_wait_until(
                &format!("remote marker {index} reaches CloudStorage"),
                || {
                    live_mode_tick_blocking().ok
                        && fs::read_to_string(&page_path)
                            .map(|contents| contents.contains(&marker))
                            .unwrap_or(false)
                },
            );
        }

        drop(cleanup);
        live_mode_e2e_wait_until("cleanup reaches CloudStorage", || {
            live_mode_tick_blocking().ok
                && fs::read_to_string(&page_path)
                    .map(|contents| !contents.contains(&run_id))
                    .unwrap_or(false)
        });
    }

    #[test]
    fn live_mode_tick_syncs_safe_pending_changes() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes = vec![PendingChange {
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
            Some(&live_mode_target("/tmp/Locality/notion/Other/page.md")),
            |change, target| {
                synced.push((change.title.clone(), target.to_path_buf()));
                Ok(())
            },
            |_| panic!("pending changes should take the live mode tick"),
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
            Some(&live_mode_target("/tmp/Locality/notion/Other/page.md")),
            |change, target| {
                synced.push((change.title.clone(), target.to_path_buf()));
                Ok(())
            },
            |_| panic!("mergeable local+remote drift should not block on remote pull cursor"),
            |change, target| {
                merged.push((change.title.clone(), target.to_path_buf()));
                Ok(true)
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
    fn live_mode_tick_pauses_for_review_required_changes() {
        let mut snapshot = sample_snapshot();
        snapshot.pending_changes = vec![PendingChange {
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
            Some(&live_mode_target("/tmp/Locality/notion/Other/page.md")),
            |_, _| {
                calls += 1;
                Ok(())
            },
            |_| panic!("review-required changes should pause before remote pulls"),
            |_, _| panic!("large review-required changes should not merge remote drift"),
        );

        assert!(!report.ok);
        assert!(report.message.contains("Launch Plan"));
        assert!(report.message.contains("needs review"));
        assert_eq!(calls, 0);
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
    fn windows_cloud_files_mount_access_root_stays_at_sync_root() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/Locality")
            .projection(ProjectionMode::WindowsCloudFiles);

        assert_eq!(
            super::mount_access_root(&mount),
            std::path::PathBuf::from("/tmp/Locality")
        );
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
            super::mount_summary(None, None, None, None).projection,
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
            home.join("Locality").join("notion-main")
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
            super::mount_summary(None, None, None, None).projection,
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
            home.join("Locality").join("notion-main")
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
        assert!(!super::recoverable_macos_file_provider_activation_error(
            "Could not load the top-level Notion folder"
        ));
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
        assert!(
            ready.rgba().chunks_exact(4).any(|pixel| {
                pixel[0] > 240 && pixel[1] > 240 && pixel[2] > 240 && pixel[3] > 200
            })
        );
        assert!(
            ready
                .rgba()
                .chunks_exact(4)
                .any(|pixel| { pixel[0] < 40 && pixel[1] < 50 && pixel[2] < 70 && pixel[3] > 200 })
        );
        assert!(review.rgba().chunks_exact(4).any(|pixel| {
            pixel[0] > 200 && pixel[1] > 100 && pixel[1] < 180 && pixel[2] < 80 && pixel[3] > 200
        }));
        assert!(
            reconnect.rgba().chunks_exact(4).any(|pixel| {
                pixel[0] > 180 && pixel[1] < 90 && pixel[2] < 90 && pixel[3] > 200
            })
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_single_instance_objects_are_session_scoped_and_null_terminated() {
        assert!(super::WINDOWS_DESKTOP_SINGLE_INSTANCE_MUTEX.starts_with(r"Local\"));
        assert!(super::WINDOWS_DESKTOP_ACTIVATION_EVENT.starts_with(r"Local\"));

        let wide = super::windows_wide_null("Locality");
        assert_eq!(wide.last().copied(), Some(0));
        assert_eq!(&wide[..3], &['A' as u16, 'F' as u16, 'S' as u16]);
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
    fn virtual_projection_refresh_signals_shared_and_mount_point_roots() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/Locality/notion-main",
        )
        .projection(ProjectionMode::MacosFileProvider);

        assert_eq!(
            virtual_projection_refresh_signal_identifiers(&mount),
            vec!["root".to_string(), "mount:notion-main".to_string()]
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
            r#""C:\Program Files\Locality\Locality.exe""#
        );
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
    fn state_watcher_refreshes_for_user_visible_state_changes() {
        let temp = TestTempDir::new("state-watch-refresh");
        let state_root = temp.path().join(".loc");
        let content_root = temp.path().join("group-content");
        let content_roots = vec![content_root.clone()];

        assert!(state_event_path_requires_refresh(
            &state_root.join("desktop-activity.json"),
            &state_root,
            &content_roots
        ));
        assert!(state_event_path_requires_refresh(
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
        assert!(items.iter().all(|item| !item.undo_available));
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
        mount: mount.clone(),
        mounts: vec![mount],
        active_mount_id: Some("notion-main".to_string()),
        needs_onboarding: false,
        settings: DesktopSettings {
            launch_at_login: true,
            show_menu_bar: true,
        },
        pending_changes: sample_pending_changes(),
        activity: vec![
            ActivityItem {
                title: "Pushed Roadmap 2026 to Notion".to_string(),
                detail: "2 block edits".to_string(),
                when: "Today".to_string(),
                occurred_at: Some("unix_ms:1782033300000".to_string()),
                kind: "push".to_string(),
                undo_available: true,
            },
            ActivityItem {
                title: "Located Launch Plan".to_string(),
                detail: "Prepared local path for an agent".to_string(),
                when: "Today".to_string(),
                occurred_at: Some("unix_ms:1782028800000".to_string()),
                kind: "locate".to_string(),
                undo_available: false,
            },
            ActivityItem {
                title: "Connected Notion workspace CodeFlash".to_string(),
                detail: "Credentials stored in the OS credential store".to_string(),
                when: "Earlier".to_string(),
                occurred_at: Some("unix_ms:1781942400000".to_string()),
                kind: "connect".to_string(),
                undo_available: false,
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
fn live_mode_e2e_page_path() -> PathBuf {
    let raw = std::env::var("LOCALITY_DESKTOP_LIVE_MODE_E2E_PAGE")
        .expect("set LOCALITY_DESKTOP_LIVE_MODE_E2E_PAGE to a scratch CloudStorage page.md path");
    let expanded = expand_tilde(&raw).unwrap_or_else(|_| PathBuf::from(raw));
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().expect("current dir").join(expanded)
    }
}

#[cfg(test)]
fn live_mode_e2e_page_id(page_path: &Path) -> String {
    if let Ok(page_id) = std::env::var("LOCALITY_DESKTOP_LIVE_MODE_E2E_PAGE_ID")
        && !page_id.trim().is_empty()
    {
        return page_id.trim().to_string();
    }
    let contents = fs::read_to_string(page_path).expect("read CloudStorage page.md");
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
        resolve_desktop_mount_path(&store, page_path).expect("resolve CloudStorage mount path");
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
            title: "Roadmap 2026".to_string(),
            local_path: "Engineering/Roadmap 2026/page.md".to_string(),
            summary: "2 text edits".to_string(),
            state: "safe".to_string(),
            issue_codes: Vec::new(),
            live_mode: sample_live_mode_status(false),
        },
        PendingChange {
            title: "Launch Plan".to_string(),
            local_path: "Marketing/Launch Plan/page.md".to_string(),
            summary: "needs review: large deletion".to_string(),
            state: "needs_review".to_string(),
            issue_codes: vec!["large_deletion".to_string()],
            live_mode: sample_live_mode_status(false),
        },
        PendingChange {
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
                build_tray(app)?;
                app.app_handle().exit(0);
                return Ok(());
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
            start_windows_cloud_files_provider_supervisor();
            start_agent_guidance_refresher();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            desktop_snapshot,
            connect_notion,
            change_notion_access,
            notion_login_link,
            install_state_review,
            acknowledge_install_state,
            reset_locality_state,
            choose_mount_folder,
            ensure_runtime_ready,
            ensure_terminal_cli_available,
            create_workspace_mount,
            create_desktop_mount,
            install_agent_guidance,
            locate_notion_page,
            search_notion_pages,
            review_push_plan,
            push_to_notion,
            push_notion_file,
            pull_notion_file,
            live_mode_tick,
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
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Locality desktop app");
}

fn should_hide_tray_popover(window_label: &str, event: &tauri::WindowEvent) -> bool {
    window_label == "tray" && matches!(event, tauri::WindowEvent::Focused(false))
}

fn start_agent_guidance_refresher() {
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(30));
        loop {
            refresh_agent_guidance_best_effort();
            std::thread::sleep(std::time::Duration::from_secs(10 * 60));
        }
    });
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

fn refresh_agent_guidance_best_effort() {
    let Some(mount_path) = agent_guidance_mount_path() else {
        return;
    };
    let report = install_guidance_files(Some(&mount_path));
    if !report.ok {
        let failed = report
            .targets
            .iter()
            .filter(|target| target.status == "failed")
            .map(|target| target.detail.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        eprintln!("loc desktop could not refresh agent guidance: {failed}");
    }
}

fn agent_guidance_mount_path() -> Option<String> {
    let store = SqliteStateStore::open(default_state_root()).ok()?;
    let mounts = store.load_mounts().ok()?;
    let mount = choose_mount(&mounts)?;
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
        .icon_as_template(false)
        .tooltip("Locality")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                position,
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_tray_popover(tray.app_handle(), position);
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
                show_main_window_with_view(app, Some("mount"));
            }
            "review_pending" => show_main_window_with_view(app, Some("pending")),
            "hide_menubar" => {
                let _ = set_menu_bar_visible(app, false);
            }
            "quit_completely" => {
                stop_windows_cloud_files_provider_supervisor();
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    if let Err(error) = build_tray_popover(app) {
        eprintln!("loc desktop could not build tray popover: {error}");
    }
    refresh_tray_icon(app.app_handle());

    Ok(())
}

fn build_tray_popover(app: &mut tauri::App) -> tauri::Result<()> {
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
    let Some(window) = app.get_webview_window("tray") else {
        show_main_window_with_view(app, None);
        return;
    };

    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        return;
    }

    refresh_tray_icon(app);
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
}

fn screen_bounds_for_tray_anchor(
    app: &AppHandle,
    position: PhysicalPosition<f64>,
) -> Option<ScreenBounds> {
    if let Ok(Some(monitor)) = app.monitor_from_point(position.x, position.y) {
        return Some(monitor_work_area_bounds(&monitor));
    }

    app.primary_monitor()
        .ok()
        .flatten()
        .map(|monitor| monitor_work_area_bounds(&monitor))
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

    if let Some(window) = app.get_webview_window("main") {
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
}
