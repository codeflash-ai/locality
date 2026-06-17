use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

use afs_core::{AfsError, AfsResult};
use afs_store::{MountConfig, MountRepository, SqliteStateStore};

use crate::DaemonConfig;
use crate::ipc::{
    DaemonBuildInfo, DaemonReloadReport, DaemonRequest, DaemonResponse, DaemonStatusReport,
    DaemonWatchStatus,
};
use crate::runtime::{DaemonRuntime, DaemonRuntimeHandle};
use crate::watcher::{FileWatcher, NotifyFileWatcher, PollingStubReadWatcher};

#[cfg(unix)]
pub fn run_foreground(config: &DaemonConfig) -> AfsResult<()> {
    std::fs::create_dir_all(&config.state_root)?;
    let runtime = DaemonRuntime::spawn(config.clone())?;
    let runtime_handle = runtime.handle();
    let watch_manager = Arc::new(Mutex::new(DaemonWatchManager::new(
        config,
        runtime_handle.clone(),
    )?));
    let reload = watch_manager
        .lock()
        .expect("daemon watch manager")
        .reload_mounts()?;
    let server = DaemonServerHandle {
        runtime: runtime_handle.clone(),
        watch_manager: Arc::clone(&watch_manager),
    };

    if let Some(addr) = config.tcp_addr {
        let listener = TcpListener::bind(addr).map_err(|error| {
            AfsError::Io(format!("failed to bind daemon TCP listener: {error}"))
        })?;
        let server = server.clone();
        thread::spawn(move || accept_tcp_connections(listener, server));
    }
    if let Some(addr) = config.mcp_addr {
        match TcpListener::bind(addr) {
            Ok(listener) => match crate::mcp::McpServerConfig::discover(&config.state_root) {
                Ok(mcp_config) => {
                    println!("afsd MCP listening on http://{addr}/mcp");
                    thread::spawn(move || crate::mcp::serve_http(listener, mcp_config));
                }
                Err(error) => {
                    eprintln!("afsd MCP disabled: {error}");
                    drop(listener);
                }
            },
            Err(error) => eprintln!("afsd MCP disabled: failed to bind {addr}: {error}"),
        }
    }
    let socket_path = crate::ipc::socket_path(&config.state_root);
    remove_stale_socket(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)
        .map_err(|error| AfsError::Io(format!("failed to bind daemon socket: {error}")))?;
    let mounts = load_mounts(config)?;
    print_startup_banner(&socket_path, &mounts, &reload.watches);

    println!("afsd is running (socket: {})", socket_path.display());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let server = server.clone();
                thread::spawn(move || handle_connection(stream, server));
            }
            Err(error) => eprintln!("afsd accept failed: {error}"),
        }
    }

    Ok(())
}

#[cfg(unix)]
fn load_mounts(config: &DaemonConfig) -> AfsResult<Vec<MountConfig>> {
    let store = SqliteStateStore::open(config.state_root.clone()).map_err(AfsError::from)?;
    store.load_mounts().map_err(AfsError::from)
}

#[cfg(unix)]
fn print_startup_banner(socket_path: &Path, mounts: &[MountConfig], watches: &DaemonWatchStatus) {
    println!("afsd listening on {}", socket_path.display());
    match mounts {
        [] => println!("afsd watching 0 mounts"),
        [mount] => println!("afsd watching 1 mount: {}", mount.root.display()),
        mounts => {
            let paths = mounts
                .iter()
                .map(|mount| mount.root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            println!("afsd watching {} mounts: {paths}", mounts.len());
        }
    }
    if watches.watched_mounts != mounts.len() {
        println!("afsd active watches: {}", watches.watched_roots.join(", "));
    }
    println!("afsd auth: {}", auth_summary(mounts));
}

#[cfg(unix)]
fn auth_summary(mounts: &[MountConfig]) -> String {
    let mut labels = mounts
        .iter()
        .map(|mount| match &mount.connection_id {
            Some(connection_id) => format!("connection {}", connection_id.0),
            None if std::env::var("NOTION_TOKEN").is_ok() => "NOTION_TOKEN env".to_string(),
            None => "missing".to_string(),
        })
        .collect::<Vec<_>>();
    labels.sort();
    labels.dedup();
    labels.join(", ")
}

#[cfg(not(unix))]
pub fn run_foreground(_config: &DaemonConfig) -> AfsResult<()> {
    Err(AfsError::Unsupported("daemon IPC on non-Unix platforms"))
}

#[cfg(unix)]
#[derive(Clone)]
struct DaemonServerHandle {
    runtime: DaemonRuntimeHandle,
    watch_manager: Arc<Mutex<DaemonWatchManager>>,
}

#[cfg(unix)]
fn accept_tcp_connections(listener: TcpListener, server: DaemonServerHandle) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let server = server.clone();
                thread::spawn(move || handle_connection(stream, server));
            }
            Err(error) => eprintln!("afsd TCP accept failed: {error}"),
        }
    }
}

#[cfg(unix)]
fn handle_connection(mut stream: impl Read + Write, server: DaemonServerHandle) {
    let response = match crate::ipc::read_request(&mut stream) {
        Ok(request) => handle_request(request, &server),
        Err(error) => DaemonResponse::error("bad_request", error.message()),
    };
    write_best_effort(&mut stream, response);
}

#[cfg(unix)]
fn handle_request(request: DaemonRequest, server: &DaemonServerHandle) -> DaemonResponse {
    match request {
        DaemonRequest::Status => match daemon_status(server) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error("status_failed", error.to_string()),
        },
        DaemonRequest::ReloadMounts => {
            let mut watch_manager = server.watch_manager.lock().expect("daemon watch manager");
            match watch_manager.reload_mounts() {
                Ok(report) => DaemonResponse::ok(report),
                Err(error) => DaemonResponse::error("reload_mounts_failed", error.to_string()),
            }
        }
        request => server.runtime.request(request),
    }
}

#[cfg(unix)]
fn daemon_status(server: &DaemonServerHandle) -> AfsResult<DaemonStatusReport> {
    let runtime = server
        .runtime
        .status()
        .map_err(|_| AfsError::InvalidState("daemon runtime is not running".to_string()))?;
    let watches = server
        .watch_manager
        .lock()
        .expect("daemon watch manager")
        .status();

    Ok(DaemonStatusReport {
        status: "ok".to_string(),
        build: DaemonBuildInfo::current(),
        runtime,
        watches,
    })
}

#[cfg(unix)]
fn write_best_effort(stream: &mut impl Write, response: DaemonResponse) {
    if let Err(error) = crate::ipc::write_response(stream, &response) {
        eprintln!("afsd response failed: {}", error.message());
    }
}

#[cfg(unix)]
struct DaemonWatchManager {
    config: DaemonConfig,
    file_watcher: NotifyFileWatcher,
    stub_read_watcher: PollingStubReadWatcher,
}

#[cfg(unix)]
impl DaemonWatchManager {
    fn new(config: &DaemonConfig, runtime: DaemonRuntimeHandle) -> AfsResult<Self> {
        let file_watcher = NotifyFileWatcher::new({
            let runtime = runtime.clone();
            move |event| {
                if runtime.file_event(event).is_err() {
                    eprintln!("afsd watcher could not submit file event: runtime stopped");
                }
            }
        })?;
        let stub_read_watcher = PollingStubReadWatcher::new(
            config.state_root.clone(),
            config.runtime_tick_interval,
            {
                let runtime = runtime.clone();
                move |event| {
                    if runtime.file_event(event).is_err() {
                        eprintln!(
                            "afsd stub read watcher could not submit file event: runtime stopped"
                        );
                    }
                }
            },
        )?;

        Ok(Self {
            config: config.clone(),
            file_watcher,
            stub_read_watcher,
        })
    }

    fn reload_mounts(&mut self) -> AfsResult<DaemonReloadReport> {
        let mut desired = load_mounts(&self.config)?
            .into_iter()
            .filter(should_watch_mount)
            .map(|mount| mount.root)
            .collect::<Vec<_>>();
        desired.sort();
        desired.dedup();

        let mut current = self.file_watcher.watched_roots();
        current.sort();
        current.dedup();

        let added = desired
            .iter()
            .filter(|root| !current.contains(root))
            .cloned()
            .collect::<Vec<_>>();
        let removed = current
            .iter()
            .filter(|root| !desired.contains(root))
            .cloned()
            .collect::<Vec<_>>();

        for root in &removed {
            self.file_watcher.unwatch_mount(root)?;
            self.stub_read_watcher.unwatch_mount(root)?;
        }
        for root in &added {
            self.file_watcher.watch_mount(root.clone())?;
            self.stub_read_watcher.watch_mount(root.clone())?;
        }

        let status = self.status();
        Ok(DaemonReloadReport {
            added: added.len(),
            removed: removed.len(),
            unchanged: desired.len().saturating_sub(added.len()),
            watches: status,
        })
    }

    fn status(&self) -> DaemonWatchStatus {
        let roots = self
            .file_watcher
            .watched_roots()
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();

        DaemonWatchStatus {
            watched_mounts: roots.len(),
            watched_roots: roots,
        }
    }
}

#[cfg(unix)]
fn should_watch_mount(mount: &MountConfig) -> bool {
    !mount.projection.uses_virtual_filesystem()
}

#[cfg(unix)]
fn remove_stale_socket(socket_path: &Path) -> AfsResult<()> {
    if !socket_path.exists() {
        return Ok(());
    }

    match UnixStream::connect(socket_path) {
        Ok(_) => Err(AfsError::InvalidState(format!(
            "daemon socket `{}` is already accepting connections",
            socket_path.display()
        ))),
        Err(_) => {
            std::fs::remove_file(socket_path)?;
            Ok(())
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use afs_core::model::MountId;
    use afs_core::pull::PullMode;
    use afs_store::{MountConfig, MountRepository, ProjectionMode, SqliteStateStore};

    use super::*;

    #[test]
    fn watch_manager_reload_adds_mounts_created_after_startup() {
        let config = test_config("reload-add");
        let runtime = DaemonRuntime::spawn(config.clone()).expect("spawn runtime");
        let mut manager =
            DaemonWatchManager::new(&config, runtime.handle()).expect("watch manager");

        let initial = manager.reload_mounts().expect("initial reload");
        assert_eq!(initial.added, 0);
        assert_eq!(initial.watches.watched_mounts, 0);

        let mount_root = temp_root("reload-mount");
        let mut store = SqliteStateStore::open(config.state_root.clone()).expect("open store");
        store
            .save_mount(MountConfig::new(
                MountId::new("notion-main"),
                "notion",
                mount_root.clone(),
            ))
            .expect("save mount");

        let report = manager.reload_mounts().expect("reload mounts");
        assert_eq!(report.added, 1);
        assert_eq!(report.removed, 0);
        assert_eq!(report.watches.watched_mounts, 1);
        assert_eq!(
            report.watches.watched_roots,
            vec![mount_root.display().to_string()]
        );
        runtime.shutdown();
    }

    #[test]
    fn watch_manager_reload_skips_virtual_projection_mounts() {
        let config = test_config("reload-skip-virtual");
        let runtime = DaemonRuntime::spawn(config.clone()).expect("spawn runtime");
        let mut manager =
            DaemonWatchManager::new(&config, runtime.handle()).expect("watch manager");

        let plain_root = temp_root("reload-plain-mount");
        let virtual_root = temp_root("reload-fuse-mount");
        let mut store = SqliteStateStore::open(config.state_root.clone()).expect("open store");
        store
            .save_mount(MountConfig::new(
                MountId::new("notion-plain"),
                "notion",
                plain_root.clone(),
            ))
            .expect("save plain mount");
        store
            .save_mount(
                MountConfig::new(MountId::new("notion-fuse"), "notion", virtual_root)
                    .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save virtual mount");

        let report = manager.reload_mounts().expect("reload mounts");

        assert_eq!(report.added, 1);
        assert_eq!(report.removed, 0);
        assert_eq!(report.watches.watched_mounts, 1);
        assert_eq!(
            report.watches.watched_roots,
            vec![plain_root.display().to_string()]
        );
        runtime.shutdown();
    }

    fn test_config(name: &str) -> DaemonConfig {
        let mut config = DaemonConfig {
            state_root: temp_root(name),
            runtime_tick_interval: Duration::from_millis(10),
            tcp_addr: None,
            mcp_addr: None,
            ..Default::default()
        };
        config.pull_scheduler.mode = PullMode::Relay;
        config
    }

    fn temp_root(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("afs-server-{name}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create temp root");
        root
    }
}
