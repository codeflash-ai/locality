use std::path::{Path, PathBuf};
use std::process::Command;

use afs_cli::connect::{BrokerOAuthConnectOptions, run_connect_notion_broker_oauth};
use afs_cli::daemon::{DaemonRunState, run_daemon_control};
use afs_cli::file_provider::{
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
    JournalRepository, MountConfig, MountRepository, ProjectionMode, SqliteStateStore,
    open_credential_store,
};
use afsd::file_provider::ROOT_CONTAINER_IDENTIFIER;
use afsd::ipc::{DaemonRequest, send_request};
use afsd::notion::resolve_notion_connector_for_path;
use serde::Serialize;
use tauri::{
    AppHandle, Manager, PhysicalPosition, Position, WebviewUrl, WebviewWindowBuilder,
    menu::{Menu, MenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DesktopSnapshot {
    health: AppHealth,
    connection: ConnectionSummary,
    mount: MountSummary,
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

#[tauri::command]
fn desktop_snapshot() -> DesktopSnapshot {
    load_desktop_snapshot().unwrap_or_else(|_| sample_snapshot())
}

#[tauri::command]
fn connect_notion() -> ActionReport {
    let state_root = default_state_root();
    std::thread::spawn(move || {
        if let Err(error) = connect_notion_with_broker(state_root) {
            eprintln!("afs desktop connect notion failed: {error}");
        }
    });

    ActionReport {
        ok: true,
        message: "Notion OAuth flow started.".to_string(),
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

    locate_notion_url(&url).ok_or_else(|| "This page is not in a mounted workspace.".to_string())
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
    if let Some(tray) = app.tray_by_id("main") {
        if let Err(error) = tray.set_visible(false) {
            return ActionReport {
                ok: false,
                message: format!("Could not hide menu bar icon: {error}"),
            };
        }
    }
    if let Some(window) = app.get_webview_window("tray") {
        let _ = window.hide();
    }

    ActionReport {
        ok: true,
        message: "AFS hidden from the menu bar.".to_string(),
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
        local_path: display_path(&mount.root),
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

fn locate_notion_url(url: &str) -> Option<LocatedItem> {
    let store = SqliteStateStore::open(default_state_root()).ok()?;
    let mounts = store.load_mounts().ok()?;
    let notion_id = notion_id_from_url(url);

    for mount in mounts {
        let entities = store.list_entities(&mount.mount_id).ok()?;
        let entity = notion_id
            .as_ref()
            .and_then(|id| {
                entities.iter().find(|entity| {
                    compact_notion_id(&entity.remote_id.0) == *id
                        || entity.path.to_string_lossy().replace('-', "").contains(id)
                })
            })
            .or_else(|| entities.first())?;
        return Some(located_item_for_entity(&mount, entity));
    }

    None
}

fn located_item_for_entity(mount: &MountConfig, entity: &EntityRecord) -> LocatedItem {
    LocatedItem {
        title: entity.title.clone(),
        kind: format!("{:?}", entity.kind),
        local_path: display_path(&mount.root.join(&entity.path)),
        state: "ready".to_string(),
    }
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

fn create_notion_workspace_mount(path: &str) -> Result<String, String> {
    let state_root = default_state_root();
    let root =
        expand_tilde(path).map_err(|error| format!("Could not resolve mount path: {error}"))?;
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

    if projection.uses_virtual_filesystem() {
        register_virtual_projection(&mount_report.mount_id, &mount_report.root)?;
        prefetch_virtual_projection_root(&state_root, &mount_report.mount_id)?;
    }

    Ok(format!(
        "Mounted Notion workspace at {} with {}.",
        mount_report.root,
        projection_label(&projection)
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

fn register_virtual_projection(mount_id: &str, root: &str) -> Result<(), String> {
    match desktop_projection_mode() {
        ProjectionMode::MacosFileProvider => register_macos_virtual_projection(mount_id, root),
        ProjectionMode::LinuxFuse => Ok(()),
        ProjectionMode::PlainFiles => Ok(()),
    }
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

fn file_provider_display_name(root: &str) -> String {
    Path::new(root)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "Notion".to_string())
}

fn connect_notion_with_broker(state_root: PathBuf) -> Result<(), String> {
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
    let options = BrokerOAuthConnectOptions {
        connection_id,
        broker_url,
        client_id: start.client_id,
        session: start.session,
        state: start.state,
        code: authorization.code,
        redirect_uri: start.redirect_uri,
    };

    run_connect_notion_broker_oauth(&mut store, credentials.as_ref(), options, &broker)
        .map(|_| ())
        .map_err(|error| error.message())
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
    let compact = url
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>()
        .to_lowercase();
    if compact.len() < 32 {
        return None;
    }

    compact
        .as_bytes()
        .windows(32)
        .last()
        .and_then(|window| std::str::from_utf8(window).ok())
        .map(str::to_string)
}

fn compact_notion_id(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect::<String>()
        .to_lowercase()
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
        .setup(|app| {
            build_tray(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            desktop_snapshot,
            connect_notion,
            create_workspace_mount,
            locate_notion_page,
            review_push_plan,
            push_to_notion,
            open_path,
            show_main_window,
            hide_menubar,
            quit_completely,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run AFS desktop app");
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
                    let _ = open_in_file_manager(&path);
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
            let _ = window.eval(&format!(
                "window.dispatchEvent(new CustomEvent('afs-open-view', {{ detail: '{}' }}));",
                escaped
            ));
        }
        let _ = window.show();
        let _ = window.set_focus();
    }
}
