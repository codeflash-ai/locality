use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use afs_cli::connect::{BrokerOAuthConnectOptions, run_connect_notion_broker_oauth};
use afs_cli::daemon::{DaemonRunState, run_daemon_control};
#[cfg(target_os = "macos")]
use afs_cli::file_provider::{
    ensure_macos_file_provider_shortcut, macos_file_provider_domain_url,
    open_macos_file_provider_domain, register_macos_file_provider_domain,
};
use afs_cli::local_oauth::run_local_oauth_authorization;
use afs_cli::mount::{MountOptions, run_mount};
use afs_cli::push::{PushOptions, PushReport, push_report_exit_code, run_push_with_daemon};
use afs_cli::status::{StatusOptions, StatusState, run_status};
use afs_core::journal::{JournalEntry, JournalStatus};
use afs_core::model::MountId;
use afs_notion::oauth::{
    DEFAULT_AFS_NOTION_OAUTH_BROKER_URL, HttpNotionOAuthBrokerClient, NotionOAuthBrokerStart,
};
use afs_store::{
    ConnectionId, ConnectionRecord, ConnectionRepository, EntityRecord, EntityRepository,
    HydrationJobRepository, JournalRepository, MountConfig, MountRepository, ProjectionMode,
    SqliteStateStore, VirtualMutationRepository, open_credential_store,
};
use afsd::file_provider::ROOT_CONTAINER_IDENTIFIER;
use afsd::ipc::{DaemonRequest, send_request};
use afsd::notion::resolve_notion_connector_for_path;
use afsd::virtual_fs::virtual_fs_content_root;
use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Manager, PhysicalPosition, Position, WebviewUrl, WebviewWindowBuilder,
    menu::{Menu, MenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri_plugin_dialog::DialogExt;

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
            launch_at_login: false,
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

#[derive(Clone, Serialize)]
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

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DesktopSettingChange {
    key: String,
    enabled: bool,
}

static CONNECT_NOTION_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

#[tauri::command]
fn desktop_snapshot() -> DesktopSnapshot {
    load_desktop_snapshot().unwrap_or_else(|_| sample_snapshot())
}

#[tauri::command]
async fn connect_notion() -> ActionReport {
    if CONNECT_NOTION_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return ActionReport {
            ok: false,
            message: "A Notion connection flow is already waiting for browser approval."
                .to_string(),
        };
    }

    let state_root = default_state_root();
    let result =
        tauri::async_runtime::spawn_blocking(move || connect_notion_with_broker(state_root))
            .await
            .map_err(|error| format!("Notion OAuth worker failed: {error}"));
    CONNECT_NOTION_IN_PROGRESS.store(false, Ordering::Release);

    match result {
        Ok(Ok(message)) => ActionReport { ok: true, message },
        Ok(Err(message)) | Err(message) => {
            eprintln!("afs desktop connect notion failed: {message}");
            ActionReport { ok: false, message }
        }
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
            validate_mount_root(&path, &default_state_root())?;
            Ok(display_path(&path))
        })
        .transpose()
}

#[tauri::command]
fn ensure_runtime_ready() -> ActionReport {
    let state_root = default_state_root();
    match ensure_daemon_running(&state_root).and_then(|_| reload_daemon_mounts(&state_root)) {
        Ok(()) => ActionReport {
            ok: true,
            message: "AFS daemon is running.".to_string(),
        },
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
fn create_workspace_mount(path: String) -> ActionReport {
    match create_notion_workspace_mount(&path) {
        Ok(report) => ActionReport {
            ok: true,
            message: report,
        },
        Err(message) => ActionReport { ok: false, message },
    }
}

#[tauri::command]
fn locate_notion_page(url: String) -> Result<LocatedItem, String> {
    if !url.contains("notion.") && !url.contains("notion.so") {
        return Err("Paste a Notion page or database URL.".to_string());
    }

    locate_notion_url(&url)
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
fn push_to_notion() -> ActionReport {
    let Ok(snapshot) = load_desktop_snapshot() else {
        return ActionReport {
            ok: false,
            message: "No AFS mount is available to push.".to_string(),
        };
    };
    let Some(change) = snapshot.pending_changes.first() else {
        return ActionReport {
            ok: true,
            message: "No pending changes to push.".to_string(),
        };
    };
    let target = expand_tilde(&format!(
        "{}/{}",
        snapshot.mount.local_path, change.local_path
    ))
    .unwrap_or_else(|_| PathBuf::from(&change.local_path));

    match push_target_direct(&target) {
        Ok(report) => ActionReport {
            ok: push_report_exit_code(&report) == 0,
            message: push_report_message(&report),
        },
        Err(message) => ActionReport { ok: false, message },
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
    let health_state = health_state(&pending_changes, connection.as_ref(), daemon_ready);

    Ok(DesktopSnapshot {
        health: AppHealth {
            state: health_state.to_string(),
            attention_count: pending_changes.len(),
        },
        connection: connection_summary(connection.as_ref()),
        mount: mount_summary(mount.as_ref(), connection.as_ref()),
        settings: desktop_settings(),
        pending_changes,
        activity: activity_from_journals(&journals, &store),
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
            local_path: "~/Documents/AFS/Notion".to_string(),
            projection: "macOS File Provider".to_string(),
            read_only: false,
            status: "not_mounted".to_string(),
        };
    };

    MountSummary {
        connector: mount.connector.clone(),
        workspace_name: connection
            .and_then(|connection| connection.workspace_name.clone())
            .unwrap_or_else(|| connector_label(&mount.connector).to_string()),
        local_path: display_path(&mount_access_root(mount)),
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
        .filter(|entry| {
            matches!(
                entry.state,
                StatusState::Dirty
                    | StatusState::Conflicted
                    | StatusState::Missing
                    | StatusState::Error
            ) || entry.pending_journal_count > 0
                || entry.failed_journal_count > 0
        })
        .map(|entry| PendingChange {
            title: entry.title.clone(),
            local_path: entry.path.clone(),
            summary: status_summary_for_entry(entry),
            state: pending_state_for_entry(entry).to_string(),
        })
        .collect()
}

fn pending_state_for_entry(entry: &afs_cli::status::StatusEntry) -> &'static str {
    if matches!(entry.state, StatusState::Conflicted) {
        "conflict"
    } else if matches!(entry.state, StatusState::Error | StatusState::Missing)
        || entry.failed_journal_count > 0
    {
        "blocked"
    } else if entry
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
        return "previous push needs attention".to_string();
    }
    if entry.pending_journal_count > 0 {
        return "push in progress".to_string();
    }
    if matches!(entry.state, StatusState::Conflicted) {
        return "conflict".to_string();
    }
    if let Some(issue) = entry.issues.first() {
        return issue.message.clone();
    }
    "local edits pending review".to_string()
}

fn activity_from_journals(
    journals: &[JournalEntry],
    store: &SqliteStateStore,
) -> Vec<ActivityItem> {
    let mut items = journals
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
) -> &'static str {
    if connection.is_some_and(|connection| connection.status != "active") {
        "reconnect_needed"
    } else if !daemon_ready {
        "stopped"
    } else if !pending_changes.is_empty() {
        "needs_review"
    } else {
        "ready"
    }
}

fn locate_notion_url(url: &str) -> Result<LocatedItem, String> {
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

    let notion_id = notion_id_from_url(url)
        .ok_or_else(|| "Paste a Notion page or database URL.".to_string())?;
    for mount in &mounts {
        if mount
            .remote_root_id
            .as_ref()
            .is_some_and(|remote_id| compact_notion_id(&remote_id.0) == notion_id)
        {
            return Ok(LocatedItem {
                title: "Notion workspace root".to_string(),
                kind: "Workspace".to_string(),
                local_path: display_path(&mount_access_root(mount)),
                state: "ready".to_string(),
            });
        }

        let entities = store
            .list_entities(&mount.mount_id)
            .map_err(|error| format!("Could not load indexed Notion pages: {error}"))?;
        if let Some(entity) = entities.iter().find(|entity| {
            compact_notion_id(&entity.remote_id.0) == notion_id
                || compact_path_id(&entity.path) == notion_id
        }) {
            return Ok(located_item_for_entity(mount, entity));
        }
    }

    Err(
        "That Notion page is not in the mounted workspace yet. Make sure it was selected during Notion authorization, then sync the workspace."
            .to_string(),
    )
}

fn located_item_for_entity(mount: &MountConfig, entity: &EntityRecord) -> LocatedItem {
    LocatedItem {
        title: entity.title.clone(),
        kind: format!("{:?}", entity.kind),
        local_path: display_path(&mount_access_root(mount).join(&entity.path)),
        state: "ready".to_string(),
    }
}

fn desktop_settings() -> DesktopSettings {
    let persisted = load_desktop_settings().unwrap_or_default();
    DesktopSettings {
        launch_at_login: launch_agent_path().is_some_and(|path| path.exists()),
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
        install_launch_agent()?;
    } else if let Some(path) = launch_agent_path()
        && path.exists()
    {
        fs::remove_file(&path).map_err(|error| {
            format!(
                "Could not remove launch agent `{}`: {error}",
                path.display()
            )
        })?;
    }

    Ok(ActionReport {
        ok: true,
        message: if enabled {
            "AFS will launch at login.".to_string()
        } else {
            "AFS will not launch at login.".to_string()
        },
    })
}

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
            return url;
        }
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
    }
}

fn connector_label(connector: &str) -> &str {
    match connector {
        "notion" => "Notion",
        "linear" => "Linear",
        _ => "Workspace",
    }
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
    let root = resolve_mount_root(path)?;
    validate_mount_root(&root, &state_root)?;
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open AFS state: {error}"))?;
    let connection_id = preferred_notion_connection_id(&store)?;
    let projection = desktop_projection_mode();

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
        register_virtual_projection(&state_root, &mount)?;
        if let Err(error) = install_virtual_projection_shortcut(&mount) {
            eprintln!("afs desktop could not create virtual projection shortcut: {error}");
        }
        prefetch_virtual_projection_root(&state_root, &mount.mount_id.0)?;
    }

    Ok(format!(
        "Mounted Notion workspace at {} with {}.",
        mount_report.root,
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
        ProjectionMode::PlainFiles
    }
}

fn ensure_daemon_running(state_root: &Path) -> Result<(), String> {
    if daemon_is_ready(state_root) {
        return Ok(());
    }

    let mut args = vec![
        "start".to_string(),
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

    let report = run_daemon_control(&args)
        .map_err(|error| format!("Could not start afsd: {}", error.message()))?;
    if report.state == DaemonRunState::Running {
        Ok(())
    } else {
        Err("afsd did not start.".to_string())
    }
}

fn reload_daemon_mounts(state_root: &Path) -> Result<(), String> {
    match send_request(state_root, &DaemonRequest::ReloadMounts) {
        Ok(response) if response.ok => Ok(()),
        Ok(response) => {
            let message = response
                .error
                .map(|error| format!("{}: {}", error.code, error.message))
                .unwrap_or_else(|| "daemon returned an unknown reload error".to_string());
            Err(format!("Could not reload afsd mounts: {message}"))
        }
        Err(error) => Err(format!("Could not reload afsd mounts: {}", error.message())),
    }
}

fn prefetch_virtual_projection_root(state_root: &Path, mount_id: &str) -> Result<(), String> {
    match send_request(
        state_root,
        &DaemonRequest::FileProviderChildren {
            mount_id: mount_id.to_string(),
            container_identifier: ROOT_CONTAINER_IDENTIFIER.to_string(),
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

fn register_virtual_projection(state_root: &Path, mount: &MountConfig) -> Result<(), String> {
    match mount.projection {
        ProjectionMode::MacosFileProvider => {
            register_macos_virtual_projection(&mount.mount_id.0, &mount.root.display().to_string())
        }
        ProjectionMode::LinuxFuse => register_linux_virtual_projection(state_root, mount),
        ProjectionMode::PlainFiles => Ok(()),
    }
}

fn install_virtual_projection_shortcut(mount: &MountConfig) -> Result<(), String> {
    match mount.projection {
        ProjectionMode::MacosFileProvider => install_macos_file_provider_shortcut(mount),
        ProjectionMode::LinuxFuse | ProjectionMode::PlainFiles => Ok(()),
    }
}

#[cfg(target_os = "macos")]
fn install_macos_file_provider_shortcut(mount: &MountConfig) -> Result<(), String> {
    ensure_macos_file_provider_shortcut(mount)
        .map(|_| ())
        .map_err(|error| {
            format!(
                "Could not create macOS File Provider shortcut: {}",
                error.message()
            )
        })
}

#[cfg(not(target_os = "macos"))]
fn install_macos_file_provider_shortcut(_mount: &MountConfig) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn register_macos_virtual_projection(mount_id: &str, root: &str) -> Result<(), String> {
    register_macos_file_provider_domain(mount_id, &file_provider_display_name(root))
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

fn open_virtual_mount_or_path(path: &Path) -> Result<(), String> {
    if let Some(mount) = virtual_mount_for_path(path)
        && mount.projection.uses_virtual_filesystem()
    {
        if open_virtual_projection(&mount).is_ok() {
            return Ok(());
        }
        return open_in_file_manager(&mount.root);
    }

    open_in_file_manager(path)
}

fn virtual_mount_for_path(path: &Path) -> Option<MountConfig> {
    let store = SqliteStateStore::open(default_state_root()).ok()?;
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    store
        .load_mounts()
        .ok()?
        .into_iter()
        .filter(|mount| target.starts_with(&mount.root))
        .max_by_key(|mount| mount.root.components().count())
}

fn open_virtual_projection(mount: &MountConfig) -> Result<(), String> {
    match mount.projection {
        ProjectionMode::MacosFileProvider => open_macos_virtual_projection(&mount.mount_id.0),
        ProjectionMode::LinuxFuse | ProjectionMode::PlainFiles => open_in_file_manager(&mount.root),
    }
}

#[cfg(target_os = "macos")]
fn open_macos_virtual_projection(mount_id: &str) -> Result<(), String> {
    open_macos_file_provider_domain(mount_id)
        .map(|_| ())
        .map_err(|error| {
            format!(
                "Could not open macOS File Provider mount: {}",
                error.message()
            )
        })
}

#[cfg(not(target_os = "macos"))]
fn open_macos_virtual_projection(_mount_id: &str) -> Result<(), String> {
    Err("macOS File Provider mounts can only be opened on macOS.".to_string())
}

#[cfg(target_os = "macos")]
fn file_provider_display_name(root: &str) -> String {
    Path::new(root)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "Notion".to_string())
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
        prefetch_virtual_projection_root(state_root, &mount.mount_id.0)?;
        wait_for_mount_entities(state_root, &mount.mount_id)?;
        register_virtual_projection(state_root, &mount)?;
        let _ = mount_access_root(&mount);
        if let Err(error) = install_virtual_projection_shortcut(&mount) {
            eprintln!("afs desktop could not refresh virtual projection shortcut: {error}");
        }
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

fn push_target_direct(target: &Path) -> Result<PushReport, String> {
    let state_root = default_state_root();
    let mut store = SqliteStateStore::open(state_root.clone())
        .map_err(|error| format!("Could not open AFS state: {error}"))?;
    let credentials = open_credential_store(&state_root);
    let connector = resolve_notion_connector_for_path(&store, credentials.as_ref(), target)
        .map_err(|error| error.message())?;

    run_push_with_daemon(
        &mut store,
        &connector,
        target,
        PushOptions {
            assume_yes: true,
            confirm_dangerous: false,
        },
    )
    .map_err(|error| error.to_string())
}

fn push_report_message(report: &PushReport) -> String {
    match report.message.as_deref() {
        Some(message) if !message.is_empty() => message.to_string(),
        _ if report.ok => "Pushed changes to Notion.".to_string(),
        _ => format!("Push stopped: {}", report.action),
    }
}

fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| std::env::var(key).ok())
        .filter(|value| !value.is_empty())
}

fn notion_id_from_url(url: &str) -> Option<String> {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    for segment in without_query.rsplit('/') {
        if let Some(candidate) = compact_notion_id_suffix(segment) {
            return Some(candidate);
        }
    }

    compact_notion_id_suffix(url)
}

fn compact_path_id(path: &Path) -> String {
    compact_notion_id_suffix(&path.to_string_lossy()).unwrap_or_default()
}

fn compact_notion_id_suffix(value: &str) -> Option<String> {
    let compact = value
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>()
        .to_lowercase();
    if compact.len() < 32 {
        return None;
    }

    Some(compact[compact.len() - 32..].to_string())
}

fn compact_notion_id(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use afs_store::{ConnectionId, ConnectionRecord};

    use super::{
        connection_metadata_changed, notion_id_from_url, should_hide_tray_popover,
        validate_mount_root,
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
    fn connection_metadata_change_detects_workspace_switches() {
        let previous = test_connection("workspace-1", "Teamspace A");
        let next = test_connection("workspace-2", "Teamspace B");

        assert!(connection_metadata_changed(Some(&previous), Some(&next)));
        assert!(!connection_metadata_changed(
            Some(&previous),
            Some(&previous)
        ));
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
            local_path: "~/Documents/AFS/Notion".to_string(),
            projection: "macOS File Provider".to_string(),
            read_only: false,
            status: "ready".to_string(),
        },
        settings: DesktopSettings {
            launch_at_login: false,
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
            local_path: "Engineering/Roadmap 2026 ~a3f2.md".to_string(),
            summary: "2 text edits".to_string(),
            state: "safe".to_string(),
        },
        PendingChange {
            title: "Launch Plan".to_string(),
            local_path: "Marketing/Launch Plan ~8841.md".to_string(),
            summary: "needs review: large deletion".to_string(),
            state: "needs_review".to_string(),
        },
        PendingChange {
            title: "Customer Notes".to_string(),
            local_path: "Sales/Customer Notes ~6b91.md".to_string(),
            summary: "1 property edit".to_string(),
            state: "safe".to_string(),
        },
    ]
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
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
            build_tray(app)?;
            if !desktop_settings().show_menu_bar
                && let Some(tray) = app.tray_by_id("main")
            {
                let _ = tray.set_visible(false);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            desktop_snapshot,
            connect_notion,
            choose_mount_folder,
            ensure_runtime_ready,
            create_workspace_mount,
            locate_notion_page,
            review_push_plan,
            push_to_notion,
            open_path,
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

fn build_tray(app: &mut tauri::App) -> tauri::Result<()> {
    build_tray_popover(app)?;

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
    let icon = app
        .default_window_icon()
        .expect("default app icon exists")
        .clone();

    TrayIconBuilder::with_id("main")
        .icon(icon)
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
                if let Some(tray) = app.tray_by_id("main") {
                    let _ = tray.set_visible(false);
                }
                if let Some(window) = app.get_webview_window("tray") {
                    let _ = window.hide();
                }
            }
            "quit_completely" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    Ok(())
}

fn build_tray_popover(app: &mut tauri::App) -> tauri::Result<()> {
    WebviewWindowBuilder::new(app, "tray", WebviewUrl::App("index.html#tray".into()))
        .title("AFS")
        .inner_size(360.0, 520.0)
        .resizable(false)
        .decorations(false)
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

    let x = (position.x - 180.0).max(8.0) as i32;
    let y = (position.y + 12.0).max(8.0) as i32;
    let _ = window.set_position(Position::Physical(PhysicalPosition::new(x, y)));
    let _ = window.show();
    let _ = window.set_focus();
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
