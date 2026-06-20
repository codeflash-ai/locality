use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use afs_cli::connect::{BrokerOAuthConnectOptions, run_connect_notion_broker_oauth};
use afs_cli::daemon::{DaemonRunState, run_daemon_control};
use afs_cli::diff::{DiffReport, run_diff};
#[cfg(target_os = "macos")]
use afs_cli::file_provider::{
    macos_file_provider_display_name, macos_file_provider_domain_url,
    open_macos_file_provider_domain, register_macos_file_provider_domain,
    run_macos_file_provider_helper,
};
#[cfg(target_os = "windows")]
use afs_cli::file_provider::{
    open_windows_cloud_files_sync_root, register_windows_cloud_files_sync_root,
};
use afs_cli::local_oauth::run_local_oauth_authorization;
use afs_cli::mount::{MountOptions, run_mount};
use afs_cli::pull::{PullReport, run_pull_with_state_root};
use afs_cli::push::{
    PushOptions, PushReport, push_report_exit_code, run_push_with_daemon_at_state_root,
};
use afs_cli::search::{
    SearchOptions, SearchResult, notion_id_from_url, run_search_with_access_roots,
};
use afs_cli::status::{StatusOptions, StatusState, StatusSyncState, run_status};
use afs_core::canonical::parse_canonical_markdown;
use afs_core::conflict::has_unresolved_conflict_markers;
use afs_core::hydration::{HydrationReason, HydrationRequest};
use afs_core::journal::{JournalEntry, JournalStatus};
use afs_core::model::{HydrationState, MountId, RemoteId};
use afs_notion::oauth::{
    DEFAULT_AFS_NOTION_OAUTH_BROKER_URL, HttpNotionOAuthBrokerClient, NotionOAuthBrokerStart,
};
use afs_platform::bundled_binary_next_to_current_exe;
use afs_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, EntityRepository, HydrationJobRecord,
    HydrationJobRepository, JournalRepository, MountConfig, MountRepository, ProjectionMode,
    ShadowRepository, SqliteStateStore, VirtualMutationRepository, open_credential_store,
};
use afsd::file_provider::{self as daemon_file_provider, ROOT_CONTAINER_IDENTIFIER};
use afsd::ipc::{DaemonBuildInfo, DaemonRequest, DaemonStatusReport, send_request};
use afsd::source::{resolve_source_for_path, source_display_name};
use afsd::virtual_fs::{
    commit_virtual_fs_write, source_root_directory_name, source_root_identifier,
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
const TERMINAL_CLI_PATH_MANAGED_START: &str = "# >>> AFS_TERMINAL_CLI_PATH >>>";
#[cfg(any(not(windows), test))]
const TERMINAL_CLI_PATH_MANAGED_END: &str = "# <<< AFS_TERMINAL_CLI_PATH <<<";
#[cfg(windows)]
const WINDOWS_TERMINAL_CLI_SHIM_MARKER: &str = "AFS_TERMINAL_CLI_SHIM";
#[cfg(windows)]
const WINDOWS_RUN_KEY_PATH: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const WINDOWS_RUN_VALUE_NAME: &str = "AFS";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopSnapshot {
    health: AppHealth,
    connection: ConnectionSummary,
    mount: MountSummary,
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
    connector: String,
    workspace_name: String,
    local_path: String,
    notion_url: Option<String>,
    projection: String,
    read_only: bool,
    status: String,
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityItem {
    title: String,
    detail: String,
    when: String,
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

static CONNECT_NOTION_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static NOTION_LOGIN_LINK: OnceLock<Mutex<Option<String>>> = OnceLock::new();
const DESKTOP_INSTALL_MARKER_VERSION: u32 = 2;
const DESKTOP_ACTIVITY_LIMIT: usize = 20;
const TRAY_POPOVER_WIDTH: f64 = 360.0;
const TRAY_POPOVER_HEIGHT: f64 = 520.0;
const TRAY_POPOVER_EDGE_MARGIN: f64 = 8.0;
const TRAY_POPOVER_ANCHOR_OFFSET: f64 = 12.0;

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

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScreenBounds {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

#[tauri::command]
fn desktop_snapshot(app: AppHandle) -> DesktopSnapshot {
    let snapshot = load_desktop_snapshot().unwrap_or_else(|_| sample_snapshot());
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
                eprintln!("afs desktop could not record Notion access activity: {error}");
            }
            ActionReport { ok: true, message }
        }
        Ok(Err(message)) | Err(message) => {
            eprintln!("afs desktop {} failed: {message}", action.failure_label());
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
fn install_state_review() -> InstallStateReview {
    inspect_install_state(&default_state_root())
}

#[tauri::command]
fn acknowledge_install_state() -> ActionReport {
    match record_current_install_marker(&default_state_root()) {
        Ok(()) => ActionReport {
            ok: true,
            message: "AFS install state recorded.".to_string(),
        },
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
fn reset_local_afs_state(app: AppHandle) -> ActionReport {
    let state_root = default_state_root();
    match reset_local_afs_state_at(&state_root)
        .and_then(|_| record_current_install_marker(&state_root))
    {
        Ok(()) => {
            refresh_desktop_surfaces(&app);
            ActionReport {
                ok: true,
                message: "AFS local state was reset. Local files were left in place.".to_string(),
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
            Ok(display_path(&root))
        })
        .transpose()
}

#[tauri::command]
fn ensure_runtime_ready(app: AppHandle) -> ActionReport {
    let state_root = default_state_root();
    match ensure_daemon_running(&state_root).and_then(|_| reload_daemon_mounts(&state_root)) {
        Ok(()) => {
            refresh_tray_icon(&app);
            ActionReport {
                ok: true,
                message: "AFS daemon is running.".to_string(),
            }
        }
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
fn ensure_terminal_cli_available() -> ActionReport {
    match install_terminal_cli_link() {
        Ok(path) => ActionReport {
            ok: true,
            message: format!("AFS terminal command is ready at {}.", path.display()),
        },
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
fn create_workspace_mount(app: AppHandle, path: String) -> ActionReport {
    match create_notion_workspace_mount(&path) {
        Ok(report) => {
            refresh_tray_icon(&app);
            ActionReport {
                ok: true,
                message: report,
            }
        }
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
fn install_agent_guidance(mount_path: Option<String>) -> AgentGuidanceInstallReport {
    install_guidance_files(mount_path.as_deref())
}

#[tauri::command]
fn locate_notion_page(url: String) -> Result<LocatedItem, String> {
    let query = url.trim();
    if query.is_empty() {
        return Err("Paste a Notion page URL or search your local Notion index.".to_string());
    }

    locate_notion_query(query)
}

#[tauri::command]
fn search_notion_pages(query: String) -> Result<Vec<LocatedItem>, String> {
    search_notion_index(&query, 8)
}

#[tauri::command]
fn review_push_plan() -> PushPlan {
    let files = load_desktop_snapshot()
        .map(|snapshot| snapshot.pending_changes)
        .unwrap_or_else(|_| sample_pending_changes());
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
            Err(message) => ActionReport { ok: false, message },
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
            message: "No AFS mount is available to push.".to_string(),
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

#[tauri::command]
fn open_path(path: String) -> ActionReport {
    let expanded = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
    match open_virtual_mount_or_path(&expanded) {
        Ok(()) => ActionReport {
            ok: true,
            message: format!("Opened {}", expanded.display()),
        },
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
fn reveal_path(path: String) -> ActionReport {
    let expanded = expand_tilde(&path).unwrap_or_else(|_| PathBuf::from(&path));
    match reveal_in_file_manager(&expanded) {
        Ok(()) => ActionReport {
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
        message: "Opened AFS.".to_string(),
    }
}

#[tauri::command]
fn hide_menubar(app: AppHandle) -> ActionReport {
    set_menu_bar_visible(&app, false).unwrap_or_else(action_error)
}

#[tauri::command]
fn set_desktop_setting(app: AppHandle, change: DesktopSettingChange) -> ActionReport {
    match change.key.as_str() {
        "launch_at_login" => set_launch_at_login(change.enabled).unwrap_or_else(action_error),
        "show_menu_bar" => set_menu_bar_visible(&app, change.enabled).unwrap_or_else(action_error),
        _ => ActionReport {
            ok: false,
            message: format!("Unknown desktop setting `{}`.", change.key),
        },
    }
}

#[tauri::command]
fn quit_completely(app: AppHandle) -> ActionReport {
    app.exit(0);
    ActionReport {
        ok: true,
        message: "AFS is quitting.".to_string(),
    }
}

fn load_desktop_snapshot() -> Result<DesktopSnapshot, String> {
    let state_root = default_state_root();
    let store = SqliteStateStore::open(state_root.clone()).map_err(|error| error.to_string())?;
    let mounts = store.load_mounts().map_err(|error| error.to_string())?;
    let connections = store
        .list_connections()
        .map_err(|error| error.to_string())?;
    let journals = store.list_journal().unwrap_or_default();
    let status = run_status(
        &store,
        StatusOptions {
            path: None,
            state_root: Some(state_root.clone()),
        },
    )
    .ok();

    let mount = choose_mount(&mounts);
    let connection = choose_connection(&connections, mount.as_ref());
    let pending_changes = status
        .as_ref()
        .map(pending_changes_from_status)
        .unwrap_or_default();
    let daemon_ready = send_request(&state_root, &DaemonRequest::Ping)
        .map(|response| response.ok)
        .unwrap_or(false);
    let health_state = health_state(
        &pending_changes,
        connection.as_ref(),
        daemon_ready,
        status.as_ref(),
    );

    Ok(DesktopSnapshot {
        health: AppHealth {
            state: health_state.to_string(),
            attention_count: pending_changes.len(),
        },
        connection: connection_summary(connection.as_ref()),
        mount: mount_summary(mount.as_ref(), connection.as_ref()),
        settings: desktop_settings(),
        pending_changes,
        activity: activity_from_journals(&journals, &store, &state_root),
        suggestions: vec![ConnectorSuggestion {
            connector: "Linear".to_string(),
            description: "Mount issues and projects as local files.".to_string(),
            state: "planned".to_string(),
        }],
    })
}

fn choose_mount(mounts: &[MountConfig]) -> Option<MountConfig> {
    mounts
        .iter()
        .find(|mount| mount.connector == "notion")
        .or_else(|| mounts.first())
        .cloned()
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
    mount: Option<&MountConfig>,
    connection: Option<&ConnectionRecord>,
) -> MountSummary {
    let Some(mount) = mount else {
        return MountSummary {
            connector: "notion".to_string(),
            workspace_name: connection
                .and_then(|connection| connection.workspace_name.clone())
                .unwrap_or_else(|| "No Notion folder".to_string()),
            local_path: display_path(&default_notion_access_root()),
            notion_url: None,
            projection: projection_label(&desktop_projection_mode()).to_string(),
            read_only: false,
            status: "not_mounted".to_string(),
        };
    };

    MountSummary {
        connector: mount.connector.clone(),
        workspace_name: connection
            .and_then(|connection| connection.workspace_name.clone())
            .unwrap_or_else(|| connector_label(&mount.connector)),
        local_path: display_path(&mount_access_root(mount)),
        notion_url: mount
            .remote_root_id
            .as_ref()
            .map(|remote_id| notion_object_url(&remote_id.0)),
        projection: projection_label(&mount.projection).to_string(),
        read_only: mount.read_only,
        status: "ready".to_string(),
    }
}

fn pending_changes_from_status(status: &afs_cli::status::StatusReport) -> Vec<PendingChange> {
    status
        .mounts
        .iter()
        .flat_map(|mount| mount.entries.iter())
        .filter(|entry| status_entry_needs_desktop_attention(entry))
        .map(|entry| PendingChange {
            title: entry.title.clone(),
            local_path: entry.path.clone(),
            summary: status_summary_for_entry(entry),
            state: pending_state_for_entry(entry).to_string(),
        })
        .collect()
}

fn pending_state_for_entry(entry: &afs_cli::status::StatusEntry) -> &'static str {
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

fn status_summary_for_entry(entry: &afs_cli::status::StatusEntry) -> String {
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

fn status_entry_needs_desktop_attention(entry: &afs_cli::status::StatusEntry) -> bool {
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

fn failed_journal_only(entry: &afs_cli::status::StatusEntry) -> bool {
    entry.failed_journal_count > 0
        && matches!(entry.state, StatusState::Clean)
        && entry.pending_journal_count == 0
        && entry
            .issues
            .iter()
            .all(|issue| matches!(issue.code.as_str(), "failed_journal" | "last_failure"))
}

fn status_issue_message<'a>(
    entry: &'a afs_cli::status::StatusEntry,
    code: &str,
) -> Option<&'a str> {
    entry
        .issues
        .iter()
        .find(|issue| issue.code == code)
        .map(|issue| issue.message.as_str())
}

fn failed_push_summary(message: &str) -> String {
    if is_remote_changed_push_message(message) || message.contains("changed since last sync") {
        return "Notion changed since last sync. Resolve pulls the latest version and may create conflict markers.".to_string();
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
                kind: "push".to_string(),
                undo_available,
            }
        })
        .collect::<Vec<_>>();
    items.extend(journal_items);
    items.truncate(8);

    if items.is_empty() {
        items.push(ActivityItem {
            title: "AFS desktop opened".to_string(),
            detail: "Ready to connect and review workspace changes".to_string(),
            when: "Today".to_string(),
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
    status: Option<&afs_cli::status::StatusReport>,
) -> &'static str {
    if connection.is_some_and(|connection| connection.status != "active") {
        "reconnect_needed"
    } else if !daemon_ready {
        "stopped"
    } else if !pending_changes.is_empty() {
        "needs_review"
    } else if status.is_some_and(|status| status.summary.checking_freshness > 0) {
        "checking_freshness"
    } else {
        "ready"
    }
}

fn refresh_tray_icon(app: &AppHandle) {
    if let Ok(snapshot) = load_desktop_snapshot() {
        refresh_tray_icon_for_snapshot(app, &snapshot);
        return;
    }

    set_tray_icon_and_tooltip(app, TrayVisualState::Reconnect, "AFS needs attention");
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

        let is_template = matches!(state, TrayVisualState::Ready);
        let _ = tray.set_icon_with_as_template(Some(tray_icon_image(state)), is_template);
        let _ = tray.set_tooltip(Some(tooltip.to_string()));
    }
}

fn sync_tray_visibility(app: &AppHandle, settings: &DesktopSettings) {
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_visible(settings.show_menu_bar);
    }
}

fn tray_state_for_health(state: &str) -> TrayVisualState {
    if state == "reconnect_needed" || state == "stopped" {
        TrayVisualState::Reconnect
    } else if state == "needs_review" {
        TrayVisualState::Review
    } else {
        TrayVisualState::Ready
    }
}

fn tray_tooltip(snapshot: &DesktopSnapshot) -> String {
    match snapshot.health.state.as_str() {
        "needs_review" => format!("AFS: {} pending changes", snapshot.health.attention_count),
        "checking_freshness" => "AFS: checking freshness".to_string(),
        "reconnect_needed" => "AFS: reconnect Notion".to_string(),
        "stopped" => "AFS: daemon stopped".to_string(),
        _ => "AFS: ready".to_string(),
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

    if matches!(state, TrayVisualState::Review | TrayVisualState::Reconnect) {
        for (start, end) in paths {
            draw_line(&mut rgba, size, start, end, 5.2, [255, 255, 255, 255]);
        }
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

fn notion_access_miss_message() -> String {
    let Ok(store) = SqliteStateStore::open(default_state_root()) else {
        return "That Notion page is not in the mounted Notion workspace yet. Make sure it was selected during Notion authorization, then sync the workspace.".to_string();
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
    let root_url = mount
        .as_ref()
        .and_then(|mount| mount.remote_root_id.as_ref())
        .map(|remote_id| notion_object_url(&remote_id.0));

    match root_url {
        Some(url) => format!(
            "That Notion page is not available in workspace `{workspace}` yet. Open the mounted root ({url}), make sure the page was selected for this Notion connection, then sync the workspace."
        ),
        None => format!(
            "That Notion page is not available in workspace `{workspace}` yet. Make sure it was selected for this Notion connection, then sync the workspace."
        ),
    }
}

fn notion_object_url(id: &str) -> String {
    format!("https://www.notion.so/{}", notion_url_id(id))
}

fn notion_url_id(id: &str) -> String {
    id.chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>()
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
        .map_err(|error| format!("Could not open AFS state: {error}"))?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Could not load AFS mounts: {error}"))?
        .into_iter()
        .filter(|mount| mount.connector == "notion")
        .collect::<Vec<_>>();
    if mounts.is_empty() {
        return Err("Create a Notion folder before locating pages.".to_string());
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
        launch_at_login: launch_at_login_enabled(),
        show_menu_bar: persisted.show_menu_bar,
    }
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
            "AFS is shown in the menu bar.".to_string()
        } else {
            "AFS is hidden from the menu bar.".to_string()
        },
    })
}

fn set_launch_at_login(enabled: bool) -> Result<ActionReport, String> {
    if enabled {
        if running_from_read_only_volume()? {
            return Err("Move AFS to Applications before enabling launch at login.".to_string());
        }
        install_launch_at_login()?;
    } else {
        uninstall_launch_at_login()?;
    }

    let mut settings = load_desktop_settings().unwrap_or_default();
    settings.launch_at_login = enabled;
    save_desktop_settings(&settings)?;

    Ok(ActionReport {
        ok: true,
        message: if enabled {
            "AFS will launch at login.".to_string()
        } else {
            "AFS will not launch at login.".to_string()
        },
    })
}

fn apply_launch_at_login_preference() -> Result<(), String> {
    let settings = load_desktop_settings().unwrap_or_default();
    if settings.launch_at_login && !running_from_read_only_volume()? {
        install_launch_at_login()?;
    }
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
        .map_err(|error| format!("Could not create AFS state folder: {error}"))?;
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
    option_env!("AFS_DESKTOP_BUILD_ID")
        .unwrap_or("unknown")
        .to_string()
}

fn current_daemon_build_id() -> String {
    DaemonBuildInfo::current().build_id
}

fn install_marker_path(state_root: &Path) -> PathBuf {
    state_root.join("desktop-install.json")
}

fn reset_local_afs_state_at(state_root: &Path) -> Result<(), String> {
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
            .map_err(|error| format!("Could not create AFS state folder: {error}"))?;
        return Ok(());
    }

    for entry in fs::read_dir(state_root)
        .map_err(|error| format!("Could not inspect AFS state folder: {error}"))?
    {
        let entry = entry.map_err(|error| format!("Could not inspect AFS state entry: {error}"))?;
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
        eprintln!(
            "afs desktop could not stop afsd during local state reset: {}",
            error.message()
        );
    }
}

fn reset_platform_projection_state() {
    #[cfg(target_os = "macos")]
    {
        if let Err(error) = run_macos_file_provider_helper("reset", Vec::new()) {
            eprintln!(
                "afs desktop could not reset macOS File Provider domains during local state reset: {}",
                error.message()
            );
        }
    }
}

fn remove_connection_secrets(state_root: &Path, secret_refs: Vec<String>) {
    let credentials = open_credential_store(state_root);
    for secret_ref in secret_refs {
        if let Err(error) = credentials.delete(&secret_ref) {
            eprintln!("afs desktop could not delete credential `{secret_ref}`: {error}");
        }
    }
}

fn remove_desktop_support_state() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let home = home_dir().map_err(|error| format!("HOME is not set: {error}"))?;
        for path in [
            home.join("Library/LaunchAgents/ai.codeflash.afs.afsd.plist"),
            home.join("Library/Group Containers/group.ai.codeflash.afs"),
            home.join("Library/Application Support/ai.codeflash.afs"),
            home.join("Library/Caches/ai.codeflash.afs"),
            home.join("Library/HTTPStorages/ai.codeflash.afs"),
            home.join("Library/Saved Application State/ai.codeflash.afs.savedState"),
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
            eprintln!(
                "afs desktop could not create state watch directory `{}`: {error}",
                state_root.display()
            );
            return;
        }

        let (tx, rx) = mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                eprintln!("afs desktop could not start state watcher: {error}");
                return;
            }
        };

        if let Err(error) = watcher.watch(&state_root, RecursiveMode::Recursive) {
            eprintln!(
                "afs desktop could not watch state directory `{}`: {error}",
                state_root.display()
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
                Ok(Err(error)) => eprintln!("afs desktop state watcher event failed: {error}"),
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
        eprintln!(
            "afs desktop could not create virtual content watch directory `{}`: {error}",
            content_root.display()
        );
        return Vec::new();
    }

    if let Err(error) = watcher.watch(&content_root, RecursiveMode::Recursive) {
        eprintln!(
            "afs desktop could not watch virtual content root `{}`: {error}",
            content_root.display()
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
            Err(error) => eprintln!("afs desktop state watcher event failed: {error}"),
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
        .is_some_and(|name| name.starts_with('.') && name.ends_with(".afs-tmp"))
}

fn refresh_desktop_surfaces(app: &AppHandle) {
    refresh_tray_icon(app);
    for label in ["main", "tray"] {
        if let Some(window) = app.get_webview_window(label) {
            if label == "main" && !window.is_visible().unwrap_or(false) {
                continue;
            }
            let _ = window.eval("window.dispatchEvent(new CustomEvent('afs-refresh-snapshot'));");
        }
    }
}

fn running_from_read_only_volume() -> Result<bool, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not resolve the AFS app executable: {error}"))?;
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
        return Err("HOME is not set, so AFS cannot install a login item.".to_string());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create launch agent folder: {error}"))?;
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not resolve the AFS app executable: {error}"))?;
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
  <string>ai.codeflash.afs.desktop</string>
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
        .map_err(|error| format!("Could not resolve the AFS app executable: {error}"))?;
    let value = windows_run_value_for_executable(&executable);
    let output = Command::new("reg")
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
    let output = Command::new("reg")
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
    let output = Command::new("reg")
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
            .join("ai.codeflash.afs.desktop.plist")
    })
}

fn action_error(message: String) -> ActionReport {
    ActionReport { ok: false, message }
}

fn mount_access_root(mount: &MountConfig) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if mount.projection == ProjectionMode::MacosFileProvider
            && let Ok(url) = macos_file_provider_domain_url(&mount.mount_id.0)
        {
            return url.join(source_root_directory_name(&mount.connector));
        }
    }

    if mount.projection == ProjectionMode::LinuxFuse {
        return mount
            .root
            .join(source_root_directory_name(&mount.connector));
    }

    mount.root.clone()
}

fn default_state_root() -> PathBuf {
    if let Ok(value) = std::env::var("AFS_STATE_DIR") {
        return PathBuf::from(value);
    }

    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".afs");
    }

    PathBuf::from(".afs")
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
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::NotFound, error))
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

fn default_notion_mount_root() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        macos_afs_cloud_storage_root().join(source_root_directory_name("notion"))
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(home) = home_dir() {
            return home.join("Documents").join("AFS");
        }
        PathBuf::from("AFS")
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        if let Ok(home) = home_dir() {
            return home.join("Documents").join("AFS").join("Notion");
        }
        PathBuf::from("AFS").join("Notion")
    }
}

fn default_notion_access_root() -> PathBuf {
    let root = default_notion_mount_root();
    if desktop_projection_mode() == ProjectionMode::LinuxFuse {
        return root.join(source_root_directory_name("notion"));
    }
    root
}

#[cfg(target_os = "macos")]
fn macos_cloud_storage_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("Library")
        .join("CloudStorage")
}

#[cfg(target_os = "macos")]
fn macos_afs_cloud_storage_root() -> PathBuf {
    macos_cloud_storage_dir().join("AFS")
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
            return Ok(macos_afs_cloud_storage_root().join(source_root_directory_name(path)));
        }
    }

    let root = resolve_mount_root(path)?;
    normalize_desktop_mount_root(&root)
}

fn normalize_desktop_mount_root(root: &Path) -> Result<PathBuf, String> {
    let root = absolute_path(root)?;

    #[cfg(target_os = "macos")]
    {
        if root == macos_cloud_storage_dir() || root == macos_afs_cloud_storage_root() {
            return Ok(default_notion_mount_root());
        }
    }

    #[cfg(target_os = "linux")]
    {
        if root
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(&source_root_directory_name("notion")))
            && let Some(parent) = root.parent()
        {
            return Ok(parent.to_path_buf());
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
            let afs_root = absolute_path(&macos_afs_cloud_storage_root())?;
            let root = absolute_path(root)?;
            if !root.starts_with(&afs_root) || root == afs_root {
                return Err(format!(
                    "Choose a source folder inside the AFS CloudStorage root, for example {}.",
                    display_path(&default_notion_mount_root())
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
        return Err("Choose a folder outside the AFS state directory.".to_string());
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

fn reveal_in_file_manager(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let target = reveal_target(path);
        Command::new("open")
            .arg("-R")
            .arg(&target)
            .spawn()
            .map_err(|error| error.to_string())?;
        Ok(())
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
            let afs_root = macos_afs_cloud_storage_root();
            if afs_root.exists() {
                dialog = dialog.set_directory(afs_root);
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

fn create_notion_workspace_mount(path: &str) -> Result<String, String> {
    let state_root = default_state_root();
    let projection = desktop_projection_mode();
    let root = resolve_desktop_mount_root(path)?;
    validate_desktop_mount_root(&root, &state_root, &projection)?;
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open AFS state: {error}"))?;
    let connection_id = preferred_notion_connection_id(&store)?;

    let mount_report = run_mount(
        &mut store,
        MountOptions {
            mount_id: MountId::new("notion-main"),
            connector: "notion".to_string(),
            root,
            remote_root_id: None,
            connection_id,
            read_only: false,
            projection: projection.clone(),
        },
    )
    .map_err(|error| error.message())?;

    ensure_daemon_running(&state_root)?;
    reload_daemon_mounts(&state_root)?;

    let mount = store
        .get_mount(&MountId::new(mount_report.mount_id.clone()))
        .map_err(|error| format!("Could not reload created mount: {error}"))?
        .ok_or_else(|| "Created mount was not found in AFS state.".to_string())?;

    if mount.projection.uses_virtual_filesystem() {
        activate_virtual_projection_mount(&state_root, &mount, false)?;
    }

    Ok(format!(
        "Mounted Notion workspace at {} with {}.",
        display_path(&mount_access_root(&mount)),
        projection_label(&mount.projection)
    ))
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
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        // Windows Cloud Files is the target product projection, but plain files
        // stay as the desktop fallback until the provider helper exists.
        ProjectionMode::PlainFiles
    }
}

fn ensure_daemon_running(state_root: &Path) -> Result<(), String> {
    let current_build = DaemonBuildInfo::current();
    match running_daemon_build(state_root) {
        Some(build) if build == current_build => return Ok(()),
        Some(build) => {
            eprintln!(
                "afs desktop detected afsd build {} but app expects {}; restarting afsd",
                build.build_id, current_build.build_id
            );
            return restart_daemon_for_current_binary(state_root);
        }
        None if daemon_is_ready(state_root) => {
            eprintln!("afs desktop detected an older afsd without build metadata; restarting afsd");
            return restart_daemon_for_current_binary(state_root);
        }
        None => {}
    }

    start_daemon_for_current_binary(state_root)
}

fn start_daemon_for_current_binary(state_root: &Path) -> Result<(), String> {
    let report = run_daemon_control(&daemon_control_args("start", state_root))
        .map_err(|error| format!("Could not start afsd: {}", error.message()))?;
    if report.state == DaemonRunState::Running {
        Ok(())
    } else {
        Err("afsd did not start.".to_string())
    }
}

fn restart_daemon_for_current_binary(state_root: &Path) -> Result<(), String> {
    let _ = run_daemon_control(&daemon_control_args_any_manager("stop", state_root));
    start_daemon_for_current_binary(state_root)?;
    match running_daemon_build(state_root) {
        Some(build) if build == DaemonBuildInfo::current() => Ok(()),
        Some(build) => Err(format!(
            "afsd restarted, but reported build {} instead of {}.",
            build.build_id,
            DaemonBuildInfo::current().build_id
        )),
        None => Err("afsd restarted, but did not report build metadata.".to_string()),
    }
}

fn daemon_control_args(action: &str, state_root: &Path) -> Vec<String> {
    let mut args = vec![
        action.to_string(),
        "--session".to_string(),
        "--state-dir".to_string(),
        state_root.display().to_string(),
    ];
    if let Ok(tcp_addr) = std::env::var("AFS_DAEMON_TCP_ADDR")
        && !tcp_addr.is_empty()
    {
        args.push("--tcp-addr".to_string());
        args.push(tcp_addr);
    }
    if let Some(afsd_bin) = bundled_afsd_binary() {
        args.push("--afsd-bin".to_string());
        args.push(afsd_bin.display().to_string());
    }
    args
}

fn daemon_control_args_any_manager(action: &str, state_root: &Path) -> Vec<String> {
    let mut args = vec![
        action.to_string(),
        "--state-dir".to_string(),
        state_root.display().to_string(),
    ];
    if let Ok(tcp_addr) = std::env::var("AFS_DAEMON_TCP_ADDR")
        && !tcp_addr.is_empty()
    {
        args.push("--tcp-addr".to_string());
        args.push(tcp_addr);
    }
    args
}

fn bundled_afsd_binary() -> Option<PathBuf> {
    bundled_binary_next_to_current_exe("afsd")
}

fn bundled_afs_cli_binary() -> Option<PathBuf> {
    bundled_binary_next_to_current_exe("afs")
}

fn app_store_distribution() -> bool {
    option_env!("AFS_DISTRIBUTION_CHANNEL")
        .is_some_and(|channel| channel.eq_ignore_ascii_case("mas"))
}

fn desktop_smoke_test_requested() -> bool {
    std::env::var_os("AFS_DESKTOP_SMOKE_TEST").is_some()
}

fn install_terminal_cli_link() -> Result<PathBuf, String> {
    if app_store_distribution() {
        if let Some(path) = find_command_in_path("afs") {
            return Ok(path);
        }
        return Err(
            "The Mac App Store build does not install a terminal command. Install AFS from Homebrew or the direct download to use the bundled CLI."
                .to_string(),
        );
    }

    if running_from_read_only_volume()? {
        if let Some(path) = find_command_in_path("afs") {
            return Ok(path);
        }
        return Err("Move AFS to Applications before installing the terminal command.".to_string());
    }

    let Some(cli_path) = bundled_afs_cli_binary() else {
        if let Some(path) = find_command_in_path("afs") {
            return Ok(path);
        }
        return Err("The packaged AFS CLI was not found in this app bundle.".to_string());
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
        "afs.cmd"
    }
    #[cfg(not(windows))]
    {
        "afs"
    }
}

fn install_terminal_cli_link_at(cli_path: &Path, link_path: &Path) -> Result<PathBuf, String> {
    let cli_path = absolute_path(cli_path)?;
    if !cli_path.is_file() {
        return Err(format!(
            "The bundled AFS CLI was not found at {}.",
            cli_path.display()
        ));
    }

    match terminal_cli_link_state(link_path, &cli_path)? {
        TerminalCliLinkState::Current => return Ok(link_path.to_path_buf()),
        TerminalCliLinkState::NeedsInstall => {}
    }

    if let Err(error) = install_terminal_cli_link_direct(&cli_path, link_path) {
        return Err(format!(
            "Could not install the AFS terminal command at {}: {error}",
            link_path.display()
        ));
    }

    match terminal_cli_link_state(link_path, &cli_path)? {
        TerminalCliLinkState::Current => Ok(link_path.to_path_buf()),
        TerminalCliLinkState::NeedsInstall => Err(format!(
            "Could not verify the AFS terminal command at {}.",
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
        "Could not install the AFS terminal command without administrator privileges. Add a user-writable directory such as ~/.local/bin to PATH, then try again. Checked PATH directories: {checked}.{detail}"
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
                local_app_data.join("AgentFS").join("bin")
            }
        })
        .or_else(|| {
            home_dir().ok().map(|home| {
                home.join("AppData")
                    .join("Local")
                    .join("AgentFS")
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
            "Installed the AFS terminal command at {}, but {} is not on PATH. Add that directory to your user PATH, then open a new terminal.",
            installed.display(),
            directory.display()
        ));
    }

    #[cfg(not(windows))]
    {
        let Some(config_path) = terminal_cli_shell_config_path() else {
            return Err(format!(
                "Installed the AFS terminal command at {}, but could not find your home directory to add it to PATH.",
                installed.display()
            ));
        };
        write_terminal_cli_path_section(&config_path, directory).map_err(|error| {
        format!(
            "Installed the AFS terminal command at {}, but could not update {} to add it to PATH: {error}",
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
_afs_cli_dir={directory}\n\
case \":$PATH:\" in\n\
  *\":$_afs_cli_dir:\"*) ;;\n\
  *) export PATH=\"$_afs_cli_dir:$PATH\" ;;\n\
esac\n\
unset _afs_cli_dir\n\
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
    value.ends_with(r"\microsoft\windowsapps") || value.ends_with(r"\agentfs\bin")
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
        "A file already exists at {}. Move it aside so AFS can install the bundled CLI there.",
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
            "A file already exists at {}. Move it aside so AFS can install the bundled CLI there.",
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
        "@echo off\r\nrem {WINDOWS_TERMINAL_CLI_SHIM_MARKER}\r\nset \"_afs_cli={cli_path}\"\r\n\"%_afs_cli%\" %*\r\n"
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
            eprintln!(
                "afs desktop detected a stale afsd schema reader during reload: {}",
                error.message
            );
            restart_daemon_for_current_binary(state_root)?;
            reload_daemon_mounts_once(state_root).map_err(|retry_error| {
                format!(
                    "Could not reload afsd mounts after restarting afsd for the current state schema: {}",
                    retry_error.message
                )
            })
        }
        Err(error) => Err(format!("Could not reload afsd mounts: {}", error.message)),
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
    register_virtual_projection(state_root, mount)?;
    prefetch_virtual_projection_root(state_root, mount)?;
    if wait_for_entities {
        wait_for_mount_entities(state_root, &mount.mount_id)?;
    }
    signal_virtual_projection_refresh(mount);
    Ok(())
}

fn prefetch_virtual_projection_root(state_root: &Path, mount: &MountConfig) -> Result<(), String> {
    prefetch_virtual_projection_container(
        state_root,
        &mount.mount_id.0,
        ROOT_CONTAINER_IDENTIFIER,
    )?;
    prefetch_virtual_projection_container(
        state_root,
        &mount.mount_id.0,
        &source_root_identifier(&mount.connector),
    )
}

fn prefetch_virtual_projection_container(
    state_root: &Path,
    mount_id: &str,
    container_identifier: &str,
) -> Result<(), String> {
    match send_request(
        state_root,
        &DaemonRequest::FileProviderChildren {
            mount_id: mount_id.to_string(),
            container_identifier: container_identifier.to_string(),
        },
    ) {
        Ok(response) if response.ok => Ok(()),
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
            "Could not ask afsd to load the top-level Notion folder: {}",
            error.message()
        )),
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
            eprintln!(
                "afs desktop could not signal {}:{} refresh: {error}",
                mount.mount_id.0, identifier
            );
        }
    }
}

fn virtual_projection_refresh_signal_identifiers(mount: &MountConfig) -> Vec<String> {
    vec![
        ROOT_CONTAINER_IDENTIFIER.to_string(),
        source_root_identifier(&mount.connector),
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
    run_macos_file_provider_helper(
        "signal",
        vec![
            "--mount-id".to_string(),
            mount_id.to_string(),
            "--identifier".to_string(),
            identifier.to_string(),
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
fn register_macos_virtual_projection(mount_id: &str, root: &str) -> Result<(), String> {
    register_macos_file_provider_domain(
        mount_id,
        &macos_file_provider_display_name(Path::new(root), "Notion"),
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
    afs_cli::file_provider::register_linux_fuse_mount(state_root, mount)
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
    match open_macos_file_provider_domain(&mount.mount_id.0) {
        Ok(_) => Ok(()),
        Err(error) => {
            let first_error = error.message();
            eprintln!(
                "afs desktop could not open macOS File Provider domain `{}`: {first_error}",
                mount.mount_id.0
            );

            if let Err(error) = register_macos_virtual_projection(
                &mount.mount_id.0,
                &mount.root.display().to_string(),
            ) {
                eprintln!("afs desktop could not re-register macOS File Provider domain: {error}");
            } else if open_macos_file_provider_domain(&mount.mount_id.0).is_ok() {
                return Ok(());
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
    open_windows_cloud_files_sync_root(mount)
        .map(|_| ())
        .map_err(|error| {
            format!(
                "Could not open Windows Cloud Files sync root: {}",
                error.message()
            )
        })
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
        .map_err(|error| format!("Could not open AFS state: {error}"))?;
    let credentials = open_credential_store(&state_root);
    let broker_url = env_first(&["AFS_NOTION_OAUTH_BROKER_URL", "AFS_AUTH_BROKER_URL"])
        .unwrap_or_else(|| DEFAULT_AFS_NOTION_OAUTH_BROKER_URL.to_string());
    let redirect_uri = env_first(&["AFS_NOTION_OAUTH_REDIRECT_URI", "NOTION_OAUTH_REDIRECT_URI"])
        .unwrap_or_else(|| "http://localhost:8757/oauth/notion/callback".to_string());
    let broker = HttpNotionOAuthBrokerClient::new(broker_url.clone());
    let start = broker
        .start(&NotionOAuthBrokerStart {
            redirect_uri: redirect_uri.clone(),
        })
        .map_err(|error| format!("Could not start Notion OAuth broker flow: {error}"))?;
    set_notion_login_link(start.authorization_url.clone());
    let authorization = run_local_oauth_authorization(
        "Notion",
        &start.authorization_url,
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
        return Ok("Create a Notion folder to mount the newly connected workspace.".to_string());
    };

    let next_connection = store
        .get_connection(&connection_id)
        .map_err(|error| format!("Could not load connected Notion metadata: {error}"))?;
    let connection_changed =
        connection_metadata_changed(previous_connection, next_connection.as_ref());

    mount.connection_id = Some(connection_id);
    store
        .save_mount(mount.clone())
        .map_err(|error| format!("Could not update Notion mount connection: {error}"))?;

    ensure_daemon_running(state_root)?;
    if mount_has_pending_local_changes(store, state_root, &mount.mount_id)? {
        reload_daemon_mounts(state_root)?;
        return Ok(
            "AFS updated the connection metadata, but kept the current mount cache because there are pending local changes to review."
                .to_string(),
        );
    }

    clear_mount_cached_projection(store, state_root, &mount.mount_id)?;
    reload_daemon_mounts(state_root)?;

    if mount.projection.uses_virtual_filesystem() {
        activate_virtual_projection_mount(state_root, &mount, true)?;
    }

    if connection_changed {
        Ok("AFS refreshed the mounted folder for the newly connected workspace.".to_string())
    } else {
        Ok("AFS refreshed the mounted folder for the latest Notion access.".to_string())
    }
}

fn connection_metadata_changed(
    previous: Option<&ConnectionRecord>,
    next: Option<&ConnectionRecord>,
) -> bool {
    previous.map(connection_metadata_key) != next.map(connection_metadata_key)
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
    state_root: &Path,
    mount_id: &MountId,
) -> Result<bool, String> {
    if !store
        .list_virtual_mutations(mount_id)
        .map_err(|error| format!("Could not inspect pending virtual changes: {error}"))?
        .is_empty()
    {
        return Ok(true);
    }

    let status = run_status(
        store,
        StatusOptions {
            path: None,
            state_root: Some(state_root.to_path_buf()),
        },
    )
    .map_err(|error| error.message())?;

    Ok(status
        .mounts
        .iter()
        .find(|mount| mount.mount_id == mount_id.0)
        .is_some_and(|mount| {
            mount.entries.iter().any(|entry| {
                matches!(entry.state, StatusState::Dirty | StatusState::Conflicted)
                    || entry.pending_journal_count > 0
                    || entry.failed_journal_count > 0
            })
        }))
}

fn clear_mount_cached_projection(
    store: &mut SqliteStateStore,
    state_root: &Path,
    mount_id: &MountId,
) -> Result<(), String> {
    let entities = store
        .list_entities(mount_id)
        .map_err(|error| format!("Could not list cached Notion items: {error}"))?;
    for entity in entities {
        store
            .delete_hydration_job(mount_id, &entity.remote_id)
            .map_err(|error| format!("Could not clear hydration job: {error}"))?;
        store
            .delete_entity(mount_id, &entity.remote_id)
            .map_err(|error| format!("Could not clear cached Notion item: {error}"))?;
    }

    for mutation in store
        .list_virtual_mutations(mount_id)
        .map_err(|error| format!("Could not list virtual mutations: {error}"))?
    {
        store
            .delete_virtual_mutation(mount_id, &mutation.local_id)
            .map_err(|error| format!("Could not clear virtual mutation: {error}"))?;
    }

    for job in store
        .list_hydration_jobs()
        .map_err(|error| format!("Could not list hydration jobs: {error}"))?
        .into_iter()
        .filter(|job| job.mount_id == *mount_id)
    {
        store
            .delete_hydration_job(mount_id, &job.remote_id)
            .map_err(|error| format!("Could not clear hydration job: {error}"))?;
    }

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
        .map_err(|error| format!("Could not open AFS state: {error}"))?;
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

fn pull_target_direct(target: &Path) -> Result<PullReport, String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open AFS state: {error}"))?;
    let credentials = open_credential_store(&state_root);
    let connector = resolve_source_for_path(&store, credentials.as_ref(), target)
        .map_err(|error| error.message())?;

    run_pull_with_state_root(&mut store, &connector, target, Some(&state_root))
        .map_err(|error| error.message())
}

fn diff_target_direct(target: &Path) -> Result<DiffReport, String> {
    let store = SqliteStateStore::open(default_state_root())
        .map_err(|error| format!("Could not open AFS state: {error}"))?;

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
        .map_err(|error| format!("Could not open AFS state: {error}"))?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Could not inspect AFS mounts: {error}"))?;
    let Some((mount, matched)) = daemon_file_provider::find_mount_for_path(&mounts, target) else {
        return Err(format!("No AFS mount contains `{}`.", target.display()));
    };
    let mount = mount.clone();
    let relative_path = matched.relative_path;
    let entity = store
        .find_entity_by_path(&mount.mount_id, &relative_path)
        .map_err(|error| format!("Could not find mounted file metadata: {error}"))?
        .ok_or_else(|| format!("No AFS file metadata found for `{}`.", target.display()))?;

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
    mut entity: afs_store::EntityRecord,
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
    entity: &afs_store::EntityRecord,
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
    entity: &afs_store::EntityRecord,
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
        .map_err(|error| format!("Could not open AFS state: {error}"))?;
    let mounts = store
        .load_mounts()
        .map_err(|error| format!("Could not inspect AFS mounts: {error}"))?;
    let Some((mount, matched)) = daemon_file_provider::find_mount_for_path(&mounts, target) else {
        return Err(format!("No AFS mount contains `{}`.", target.display()));
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
    let tmp = path.with_extension("tmp-afs-desktop");
    fs::write(&tmp, contents)?;
    fs::rename(tmp, path)
}

fn push_report_message(report: &PushReport) -> String {
    push_action_message(report.action.as_str(), report.ok, report.message.as_deref())
}

fn push_action_message(action: &str, ok: bool, message: Option<&str>) -> String {
    match message {
        Some(message) if is_remote_changed_push_message(message) => {
            "Notion has newer changes than your last sync. Click Resolve on this file to pull the latest version, resolve any conflict markers if AFS writes them, then push again.".to_string()
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
            "This mount is read-only, so AFS cannot push local edits to Notion.".to_string()
        }
        _ => format!("Push stopped: {action}"),
    }
}

fn pull_report_message(report: &PullReport) -> String {
    if !report.conflicts.is_empty() {
        return "Pulled the latest Notion version and wrote conflict markers into the local file. Open the file, resolve the markers, then push again.".to_string();
    }
    if report.hydrated > 0 {
        return "Synced the latest Notion version for this file.".to_string();
    }
    if report.skipped_dirty > 0 {
        return "AFS kept your local edits because the file is still dirty. Review the diff, then push or restore the file.".to_string();
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use afs_cli::search::{SearchRemoteState, SearchResult, SearchSafety};
    use afs_core::canonical::render_canonical_markdown;
    use afs_core::model::{CanonicalDocument, EntityKind, HydrationState, MountId, RemoteId};
    use afs_core::shadow::ShadowDocument;
    use afs_store::{
        ConnectionId, ConnectionRecord, EntityRecord, EntityRepository, MountConfig,
        MountRepository, ProjectionMode, ShadowRepository, SqliteStateStore,
    };
    use tauri::{PhysicalPosition, PhysicalSize};

    use super::{
        DESKTOP_ACTIVITY_LIMIT, DESKTOP_INSTALL_MARKER_VERSION, ScreenBounds, TerminalCliLinkState,
        TrayVisualState, clear_state_root_contents, conflict_preview, connection_metadata_changed,
        current_daemon_build_id, current_desktop_build_id, diff_report_message,
        failed_push_summary, hydration_after_editor_write, inspect_install_state,
        install_terminal_cli_link_at, install_terminal_cli_link_in_path_dirs,
        is_unsupported_schema_version_message, load_desktop_activity, notion_id_from_url,
        pending_changes_from_status, pull_report_message, push_action_message,
        record_current_install_marker, record_desktop_activity, shell_single_quote,
        should_hide_tray_popover, should_prioritize_located_result,
        state_event_path_requires_refresh, terminal_cli_link_state, tray_icon_image,
        tray_popover_position, validate_mount_root, virtual_projection_refresh_signal_identifiers,
        write_terminal_cli_path_section,
    };

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
            validate_mount_root(&file, &temp.path().join(".afs")).expect_err("file rejected");

        assert!(error.contains("not a file"));
    }

    #[test]
    fn mount_validation_rejects_state_directory() {
        let temp = TestTempDir::new("state-dir");
        let state_root = temp.path().join(".afs");
        fs::create_dir_all(&state_root).expect("create state dir");

        let error = validate_mount_root(&state_root, &state_root).expect_err("state dir rejected");

        assert!(error.contains("outside the AFS state directory"));
    }

    #[test]
    fn mount_validation_accepts_new_child_under_existing_parent() {
        let temp = TestTempDir::new("new-child");
        let root = temp.path().join("Notion");

        validate_mount_root(&root, &temp.path().join(".afs")).expect("valid child path");
    }

    #[test]
    fn linux_fuse_mount_access_root_points_at_connector_directory() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/AFS")
            .projection(ProjectionMode::LinuxFuse);

        assert_eq!(
            super::mount_access_root(&mount),
            std::path::PathBuf::from("/tmp/AFS/notion")
        );
    }

    #[test]
    fn plain_mount_access_root_stays_at_mount_root() {
        let mount = MountConfig::new(MountId::new("notion-main"), "notion", "/tmp/AFS")
            .projection(ProjectionMode::PlainFiles);

        assert_eq!(
            super::mount_access_root(&mount),
            std::path::PathBuf::from("/tmp/AFS")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_default_notion_mount_root_is_shared_afs_root() {
        let home = super::home_dir().expect("home dir");

        assert_eq!(
            super::default_notion_mount_root(),
            home.join("Documents").join("AFS")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_default_notion_access_root_is_connector_directory() {
        let home = super::home_dir().expect("home dir");

        assert_eq!(
            super::default_notion_access_root(),
            home.join("Documents").join("AFS").join("notion")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_mount_summary_without_mount_reports_linux_fuse_projection() {
        assert_eq!(super::mount_summary(None, None).projection, "Linux FUSE");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_desktop_mount_normalizes_selected_connector_directory_to_shared_root() {
        let home = super::home_dir().expect("home dir");
        let selected = home.join("Documents").join("AFS").join("notion");

        assert_eq!(
            super::resolve_desktop_mount_root(&selected.display().to_string())
                .expect("resolve mount root"),
            home.join("Documents").join("AFS")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_desktop_mount_resolves_bare_name_under_cloudstorage() {
        let root = super::resolve_desktop_mount_root("Notion").expect("resolve mount root");

        assert_eq!(root, super::macos_afs_cloud_storage_root().join("notion"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_desktop_mount_rejects_paths_outside_cloudstorage() {
        let temp = TestTempDir::new("desktop-mount-outside-cloudstorage");
        let root = temp.path().join("Notion");

        let error = super::validate_desktop_mount_root(
            &root,
            &temp.path().join(".afs"),
            &afs_store::ProjectionMode::MacosFileProvider,
        )
        .expect_err("non-CloudStorage path rejected");

        assert!(error.contains("inside the AFS CloudStorage root"));
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
        assert!(review.rgba().chunks_exact(4).any(|pixel| {
            pixel[0] > 200 && pixel[1] > 100 && pixel[1] < 180 && pixel[2] < 80 && pixel[3] > 200
        }));
        assert!(
            reconnect.rgba().chunks_exact(4).any(|pixel| {
                pixel[0] > 180 && pixel[1] < 90 && pixel[2] < 90 && pixel[3] > 200
            })
        );
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
    fn virtual_projection_refresh_signals_shared_and_connector_roots() {
        let mount = MountConfig::new(
            MountId::new("notion-main"),
            "notion",
            "/tmp/CloudStorage/AFS/notion",
        )
        .projection(ProjectionMode::MacosFileProvider);

        assert_eq!(
            virtual_projection_refresh_signal_identifiers(&mount),
            vec!["root".to_string(), "source:notion".to_string()]
        );
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

        assert!(message.contains("Click Resolve"));
        assert!(message.contains("pull the latest version"));
    }

    #[test]
    fn failed_push_summary_explains_remote_changed_recovery() {
        let message = failed_push_summary(
            "guardrail blocked push: remote entity `abc` changed since last sync (expected remote_edited_at `2026-06-17T05:45:00.000Z`, found `2026-06-17T06:21:00.000Z`)",
        );

        assert!(message.contains("Notion changed since last sync"));
        assert!(message.contains("Resolve pulls"));
    }

    #[test]
    fn pending_changes_hide_clean_failed_journal_audit_entries() {
        let status = status_report_with_entry(status_entry(
            afs_cli::status::StatusState::Clean,
            afs_cli::status::StatusSyncState::ReviewNeeded,
            1,
            vec![
                status_issue("failed_journal", "1 push journal(s) failed"),
                status_issue("last_failure", "previous failure"),
            ],
        ));

        assert!(pending_changes_from_status(&status).is_empty());
    }

    #[test]
    fn pending_changes_keep_failed_journal_with_dirty_file() {
        let status = status_report_with_entry(status_entry(
            afs_cli::status::StatusState::Dirty,
            afs_cli::status::StatusSyncState::ReviewNeeded,
            1,
            vec![
                status_issue("failed_journal", "1 push journal(s) failed"),
                status_issue("last_failure", "previous failure"),
            ],
        ));

        let changes = pending_changes_from_status(&status);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].state, "blocked");
        assert!(changes[0].summary.contains("Previous push failed"));
    }

    #[test]
    fn pull_report_message_explains_conflict_markers() {
        let report = afs_cli::pull::PullReport {
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
            conflicts: vec![afsd::pull::PullConflict {
                path: "page.md".to_string(),
                remote_id: "abc".to_string(),
            }],
        };

        assert!(pull_report_message(&report).contains("conflict markers"));
    }

    fn status_report_with_entry(
        entry: afs_cli::status::StatusEntry,
    ) -> afs_cli::status::StatusReport {
        afs_cli::status::StatusReport {
            ok: true,
            clean: false,
            command: "status",
            target: None,
            summary: afs_cli::status::StatusSummary::default(),
            mounts: vec![afs_cli::status::StatusMountReport {
                mount_id: "notion-main".to_string(),
                connector: "notion".to_string(),
                root: "/tmp/notion".to_string(),
                entries: vec![entry],
            }],
        }
    }

    fn status_entry(
        state: afs_cli::status::StatusState,
        sync_state: afs_cli::status::StatusSyncState,
        failed_journal_count: usize,
        issues: Vec<afs_cli::status::StatusIssue>,
    ) -> afs_cli::status::StatusEntry {
        afs_cli::status::StatusEntry {
            path: "page.md".to_string(),
            absolute_path: "/tmp/notion/page.md".to_string(),
            entity_id: "abc".to_string(),
            kind: "page".to_string(),
            title: "Page".to_string(),
            hydration: "hydrated".to_string(),
            state,
            sync_state,
            remote: afs_cli::status::StatusRemoteState::default(),
            issues,
            pending_journal_count: 0,
            failed_journal_count,
        }
    }

    fn status_issue(code: &str, message: &str) -> afs_cli::status::StatusIssue {
        afs_cli::status::StatusIssue {
            code: code.to_string(),
            message: message.to_string(),
        }
    }

    #[test]
    fn diff_report_message_handles_noop() {
        let report = afs_cli::diff::DiffReport {
            ok: true,
            command: "diff",
            path: "/tmp/notion/page.md".to_string(),
            mount_id: "notion-main".to_string(),
            entity_id: "abc".to_string(),
            validation: Vec::new(),
            plan: None,
            guardrail: afs_cli::diff::GuardrailOutput {
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
        let frontmatter = "afs:\n  id: page-1\n  type: page\ntitle: Page\n";
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
        let cli = temp.path().join("AFS.app/Contents/MacOS/afs");
        let link = temp.path().join("bin/afs");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::write(&cli, b"afs cli").expect("write cli");

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
        let cli = temp.path().join("app/afs.exe");
        let link = temp.path().join("bin/afs.cmd");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::write(&cli, b"afs cli").expect("write cli");

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
        let old_cli = temp.path().join("old/afs.exe");
        let new_cli = temp.path().join("new/afs.exe");
        let link = temp.path().join("bin/afs.cmd");
        fs::create_dir_all(old_cli.parent().expect("old cli parent")).expect("create old parent");
        fs::create_dir_all(new_cli.parent().expect("new cli parent")).expect("create new parent");
        fs::create_dir_all(link.parent().expect("link parent")).expect("create link parent");
        fs::write(&old_cli, b"old afs cli").expect("write old cli");
        fs::write(&new_cli, b"new afs cli").expect("write new cli");
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
        let cli = temp.path().join("app/afs.exe");
        let writable = temp.path().join("writable-bin");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::create_dir_all(&writable).expect("create writable dir");
        fs::write(&cli, b"afs cli").expect("write cli");

        let installed = install_terminal_cli_link_in_path_dirs(&cli, vec![writable.clone()])
            .expect("install cli shim in path");

        assert_eq!(installed, writable.join("afs.cmd"));
        assert!(installed.is_file());
    }

    #[cfg(windows)]
    #[test]
    fn windows_terminal_lookup_checks_exe_and_cmd_extensions() {
        assert_eq!(
            super::command_path_candidates("afs"),
            vec!["afs.exe", "afs.cmd", "afs.bat", "afs"]
        );
        assert_eq!(super::terminal_cli_command_filename(), "afs.cmd");
    }

    #[cfg(windows)]
    #[test]
    fn windows_login_run_value_quotes_executable() {
        assert_eq!(
            super::windows_run_value_for_executable(Path::new(r"C:\Program Files\AFS\AFS.exe")),
            r#""C:\Program Files\AFS\AFS.exe""#
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cli_installer_updates_stale_symlink() {
        let temp = TestTempDir::new("terminal-cli-refresh");
        let old_cli = temp.path().join("old/afs");
        let new_cli = temp.path().join("new/afs");
        let link = temp.path().join("bin/afs");
        fs::create_dir_all(old_cli.parent().expect("old cli parent")).expect("create old parent");
        fs::create_dir_all(new_cli.parent().expect("new cli parent")).expect("create new parent");
        fs::create_dir_all(link.parent().expect("link parent")).expect("create link parent");
        fs::write(&old_cli, b"old afs cli").expect("write old cli");
        fs::write(&new_cli, b"new afs cli").expect("write new cli");
        std::os::unix::fs::symlink(&old_cli, &link).expect("create stale link");

        install_terminal_cli_link_at(&new_cli, &link).expect("refresh cli link");

        assert_eq!(fs::read_link(&link).expect("read cli link"), new_cli);
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cli_installer_does_not_replace_regular_file() {
        let temp = TestTempDir::new("terminal-cli-existing-file");
        let cli = temp.path().join("app/afs");
        let link = temp.path().join("bin/afs");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::create_dir_all(link.parent().expect("link parent")).expect("create link parent");
        fs::write(&cli, b"afs cli").expect("write cli");
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
        let cli = temp.path().join("app/afs");
        let occupied = temp.path().join("occupied-bin");
        let writable = temp.path().join("writable-bin");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::create_dir_all(&occupied).expect("create occupied dir");
        fs::create_dir_all(&writable).expect("create writable dir");
        fs::write(&cli, b"afs cli").expect("write cli");
        fs::write(occupied.join("afs"), b"existing command").expect("write occupied command");

        let installed =
            install_terminal_cli_link_in_path_dirs(&cli, vec![occupied.clone(), writable.clone()])
                .expect("install cli link in path");

        assert_eq!(installed, writable.join("afs"));
        assert_eq!(fs::read_link(&installed).expect("read cli link"), cli);
        assert_eq!(
            fs::read_to_string(occupied.join("afs")).expect("read occupied command"),
            "existing command"
        );
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cli_installer_prefers_user_path_before_homebrew_fallback() {
        let temp = TestTempDir::new("terminal-cli-user-path");
        let cli = temp.path().join("app/afs");
        let homebrew = temp.path().join("opt/homebrew/bin");
        let user_bin = temp.path().join(".local/bin");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::create_dir_all(&homebrew).expect("create homebrew dir");
        fs::create_dir_all(&user_bin).expect("create user dir");
        fs::write(&cli, b"afs cli").expect("write cli");

        let installed =
            install_terminal_cli_link_in_path_dirs(&cli, vec![homebrew.clone(), user_bin.clone()])
                .expect("install cli link in user path");

        assert_eq!(installed, user_bin.join("afs"));
        assert_eq!(fs::read_link(&installed).expect("read cli link"), cli);
        assert!(!homebrew.join("afs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn terminal_cli_installer_uses_user_fallback_before_protected_system_paths() {
        let temp = TestTempDir::new("terminal-cli-user-fallback");
        let cli = temp.path().join("app/afs");
        let user_bin = temp.path().join(".local/bin");
        fs::create_dir_all(cli.parent().expect("cli parent")).expect("create cli parent");
        fs::write(&cli, b"afs cli").expect("write cli");

        let mut dirs = vec![PathBuf::from("/usr/bin"), PathBuf::from("/sbin")];
        super::insert_user_terminal_cli_fallback_dir(&mut dirs, &user_bin);

        let installed = super::install_terminal_cli_link_in_sorted_path_dirs(&cli, dirs)
            .expect("install fallback");

        assert_eq!(installed, user_bin.join("afs"));
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
        assert_eq!(contents.matches("AFS_TERMINAL_CLI_PATH").count(), 2);
        assert!(contents.contains("export PATH=\"$_afs_cli_dir:$PATH\""));
    }

    #[test]
    fn terminal_cli_shell_quote_escapes_single_quotes() {
        assert_eq!(shell_single_quote("/tmp/it's/afs"), "'/tmp/it'\"'\"'s/afs'");
    }

    #[test]
    fn state_clear_removes_metadata_but_preserves_state_root() {
        let temp = TestTempDir::new("clear-state-root");
        let state_root = temp.path().join(".afs");
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
        let state_root = temp.path().join(".afs");
        let content_root = temp.path().join("group-content");
        let content_roots = vec![content_root];

        for path in [
            state_root.join("state.sqlite3"),
            state_root.join("state.sqlite3-wal"),
            state_root.join("state.sqlite3-shm"),
            state_root.join("desktop.json"),
            state_root.join("desktop-install.json"),
            state_root.join("logs/afsd.log"),
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
        let state_root = temp.path().join(".afs");
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
            &content_root.join("notion-main/files/Roadmap/.page.md.afs-tmp"),
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
                "Connected Notion workspace. AFS refreshed the mounted folder.",
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
                "afs-desktop-{label}-{}-{nanos}",
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
            absolute_path: "/tmp/afs/Roadmap/page.md".to_string(),
            state: state.to_string(),
            safety: SearchSafety {
                agent_readable: state == "ready",
                labels: vec![state.to_string()],
            },
            remote: SearchRemoteState::default(),
            score: 0,
        }
    }
}

fn sample_snapshot() -> DesktopSnapshot {
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
        mount: MountSummary {
            connector: "notion".to_string(),
            workspace_name: "CodeFlash".to_string(),
            local_path: display_path(&default_notion_mount_root()),
            notion_url: Some("https://www.notion.so/37b3ac0ebb88802cbcf4d53c9cfc4972".to_string()),
            projection: "macOS File Provider".to_string(),
            read_only: false,
            status: "ready".to_string(),
        },
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
                kind: "push".to_string(),
                undo_available: true,
            },
            ActivityItem {
                title: "Located Launch Plan".to_string(),
                detail: "Prepared local path for an agent".to_string(),
                when: "Today".to_string(),
                kind: "locate".to_string(),
                undo_available: false,
            },
            ActivityItem {
                title: "Connected Notion workspace CodeFlash".to_string(),
                detail: "Credentials stored in the OS credential store".to_string(),
                when: "Earlier".to_string(),
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

fn sample_pending_changes() -> Vec<PendingChange> {
    vec![
        PendingChange {
            title: "Roadmap 2026".to_string(),
            local_path: "Engineering/Roadmap 2026/page.md".to_string(),
            summary: "2 text edits".to_string(),
            state: "safe".to_string(),
        },
        PendingChange {
            title: "Launch Plan".to_string(),
            local_path: "Marketing/Launch Plan/page.md".to_string(),
            summary: "needs review: large deletion".to_string(),
            state: "needs_review".to_string(),
        },
        PendingChange {
            title: "Customer Notes".to_string(),
            local_path: "Sales/Customer Notes/page.md".to_string(),
            summary: "1 property edit".to_string(),
            state: "safe".to_string(),
        },
    ]
}

fn main() {
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
        .setup(|app| {
            if desktop_smoke_test_requested() {
                app.app_handle().exit(0);
                return Ok(());
            }
            if let Err(error) = apply_launch_at_login_preference() {
                eprintln!("afs desktop could not apply launch-at-login preference: {error}");
            }
            build_tray(app)?;
            sync_tray_visibility(app.app_handle(), &desktop_settings());
            start_state_change_watcher(app.app_handle().clone());
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
            reset_local_afs_state,
            choose_mount_folder,
            ensure_runtime_ready,
            ensure_terminal_cli_available,
            create_workspace_mount,
            install_agent_guidance,
            locate_notion_page,
            search_notion_pages,
            review_push_plan,
            push_to_notion,
            push_notion_file,
            pull_notion_file,
            diff_notion_file,
            inspect_notion_file,
            read_notion_file,
            save_notion_file,
            open_path,
            reveal_path,
            show_main_window,
            set_desktop_setting,
            hide_menubar,
            quit_completely,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run AFS desktop app");
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
        eprintln!("afs desktop could not refresh agent guidance: {failed}");
    }
}

fn agent_guidance_mount_path() -> Option<String> {
    let store = SqliteStateStore::open(default_state_root()).ok()?;
    let mounts = store.load_mounts().ok()?;
    let mount = choose_mount(&mounts)?;
    Some(display_path(&mount_access_root(&mount)))
}

fn build_tray(app: &mut tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open AFS", true, None::<&str>)?;
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
        .icon_as_template(true)
        .tooltip("AFS")
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
            "quit_completely" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    if let Err(error) = build_tray_popover(app) {
        eprintln!("afs desktop could not build tray popover: {error}");
    }
    refresh_tray_icon(app.app_handle());

    Ok(())
}

fn build_tray_popover(app: &mut tauri::App) -> tauri::Result<()> {
    WebviewWindowBuilder::new(app, "tray", WebviewUrl::App("index.html#tray".into()))
        .title("AFS")
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
    let _ = window.eval("window.dispatchEvent(new CustomEvent('afs-refresh-snapshot'));");
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
                "window.dispatchEvent(new CustomEvent('afs-open-view', {{ detail: '{}' }}));",
                escaped
            ));
        }
        let _ = window.show();
        let _ = window.set_focus();
    }
}
