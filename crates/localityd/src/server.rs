use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

use locality_core::{LocalityError, LocalityResult};
use locality_store::{MountConfig, MountRepository, SqliteStateStore};
use serde_json::json;

use crate::DaemonConfig;
use crate::ipc::{
    DaemonBuildInfo, DaemonReloadReport, DaemonRequest, DaemonResponse, DaemonStatusReport,
    DaemonWatchStatus,
};
use crate::runtime::{DaemonRuntime, DaemonRuntimeHandle};
use crate::watcher::{FileWatcher, NotifyFileWatcher, PollingStubReadWatcher};

pub fn run_foreground(config: &DaemonConfig) -> LocalityResult<()> {
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
    if runtime_handle.prime_virtual_mounts().is_err() {
        eprintln!("localityd could not queue virtual filesystem priming: runtime stopped");
    }
    let server = DaemonServerHandle {
        runtime: runtime_handle.clone(),
        watch_manager: Arc::clone(&watch_manager),
        shutdown: Arc::new(AtomicBool::new(false)),
    };

    if let Some(addr) = config.tcp_addr {
        let listener = TcpListener::bind(addr).map_err(|error| {
            LocalityError::Io(format!("failed to bind daemon TCP listener: {error}"))
        })?;
        listener.set_nonblocking(true).map_err(|error| {
            LocalityError::Io(format!("failed to configure daemon TCP listener: {error}"))
        })?;
        let server = server.clone();
        #[cfg(unix)]
        thread::spawn(move || accept_tcp_connections(listener, server));
        #[cfg(not(unix))]
        {
            start_mcp_listener(config);
            let mounts = load_mounts(config)?;
            print_startup_banner(None, Some(addr), &mounts, &reload.watches);
            println!("localityd is running (tcp: {addr})");
            accept_tcp_connections(listener, server);
            return Ok(());
        }
    } else if cfg!(not(unix)) {
        return Err(LocalityError::Unsupported(
            "daemon TCP IPC is required on non-Unix platforms",
        ));
    }
    start_mcp_listener(config);

    #[cfg(unix)]
    {
        let socket_path = crate::ipc::socket_path(&config.state_root);
        remove_stale_socket(&socket_path)?;
        let listener = UnixListener::bind(&socket_path)
            .map_err(|error| LocalityError::Io(format!("failed to bind daemon socket: {error}")))?;
        listener.set_nonblocking(true).map_err(|error| {
            LocalityError::Io(format!("failed to configure daemon socket: {error}"))
        })?;
        let mounts = load_mounts(config)?;
        print_startup_banner(
            Some(&socket_path),
            config.tcp_addr,
            &mounts,
            &reload.watches,
        );

        println!("localityd is running (socket: {})", socket_path.display());
        while !server.shutdown_requested() {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    let server = server.clone();
                    thread::spawn(move || handle_connection(stream, server));
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) => eprintln!("localityd accept failed: {error}"),
            }
        }
        let _ = std::fs::remove_file(&socket_path);

        Ok(())
    }
    #[cfg(not(unix))]
    {
        unreachable!("non-Unix daemon startup returns from the TCP listener branch")
    }
}

fn load_mounts(config: &DaemonConfig) -> LocalityResult<Vec<MountConfig>> {
    let store = SqliteStateStore::open(config.state_root.clone()).map_err(LocalityError::from)?;
    store.load_mounts().map_err(LocalityError::from)
}

fn start_mcp_listener(config: &DaemonConfig) {
    if let Some(addr) = config.mcp_addr {
        match TcpListener::bind(addr) {
            Ok(listener) => match crate::mcp::McpServerConfig::discover(&config.state_root) {
                Ok(mcp_config) => {
                    println!("localityd MCP listening on http://{addr}/mcp");
                    thread::spawn(move || crate::mcp::serve_http(listener, mcp_config));
                }
                Err(error) => {
                    eprintln!("localityd MCP disabled: {error}");
                    drop(listener);
                }
            },
            Err(error) => eprintln!("localityd MCP disabled: failed to bind {addr}: {error}"),
        }
    }
}

fn print_startup_banner(
    socket_path: Option<&Path>,
    tcp_addr: Option<std::net::SocketAddr>,
    mounts: &[MountConfig],
    watches: &DaemonWatchStatus,
) {
    if let Some(socket_path) = socket_path {
        println!("localityd listening on {}", socket_path.display());
    }
    if let Some(tcp_addr) = tcp_addr {
        println!("localityd TCP listening on {tcp_addr}");
    }
    match mounts {
        [] => println!("localityd watching 0 mounts"),
        [mount] => println!("localityd watching 1 mount: {}", mount.root.display()),
        mounts => {
            let paths = mounts
                .iter()
                .map(|mount| mount.root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            println!("localityd watching {} mounts: {paths}", mounts.len());
        }
    }
    if watches.watched_mounts != mounts.len() {
        println!(
            "localityd active watches: {}",
            watches.watched_roots.join(", ")
        );
    }
    println!("localityd auth: {}", auth_summary(mounts));
}

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

#[derive(Clone)]
struct DaemonServerHandle {
    runtime: DaemonRuntimeHandle,
    watch_manager: Arc<Mutex<DaemonWatchManager>>,
    shutdown: Arc<AtomicBool>,
}

impl DaemonServerHandle {
    fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }
}

fn accept_tcp_connections(listener: TcpListener, server: DaemonServerHandle) {
    while !server.shutdown_requested() {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let server = server.clone();
                thread::spawn(move || handle_connection(stream, server));
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => eprintln!("localityd TCP accept failed: {error}"),
        }
    }
}

fn handle_connection(mut stream: impl Read + Write, server: DaemonServerHandle) {
    let response = match crate::ipc::read_request(&mut stream) {
        Ok(request) => handle_request(request, &server),
        Err(error) => DaemonResponse::error("bad_request", error.message()),
    };
    write_best_effort(&mut stream, response);
}

fn handle_request(request: DaemonRequest, server: &DaemonServerHandle) -> DaemonResponse {
    match request {
        DaemonRequest::Status => match daemon_status(server) {
            Ok(report) => DaemonResponse::ok(report),
            Err(error) => DaemonResponse::error("status_failed", error.to_string()),
        },
        DaemonRequest::ReloadMounts => {
            let mut watch_manager = server.watch_manager.lock().expect("daemon watch manager");
            match watch_manager.reload_mounts() {
                Ok(report) => {
                    if server.runtime.prime_virtual_mounts().is_err() {
                        eprintln!(
                            "localityd could not queue virtual filesystem priming after mount reload: runtime stopped"
                        );
                    }
                    DaemonResponse::ok(report)
                }
                Err(error) => DaemonResponse::error("reload_mounts_failed", error.to_string()),
            }
        }
        DaemonRequest::Shutdown => {
            server.shutdown.store(true, Ordering::SeqCst);
            let _ = server.runtime.request(DaemonRequest::Shutdown);
            DaemonResponse::ok(json!({ "status": "shutting_down" }))
        }
        request => server.runtime.request(request),
    }
}

fn daemon_status(server: &DaemonServerHandle) -> LocalityResult<DaemonStatusReport> {
    let runtime = server
        .runtime
        .status()
        .map_err(|_| LocalityError::InvalidState("daemon runtime is not running".to_string()))?;
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

fn write_best_effort(stream: &mut impl Write, response: DaemonResponse) {
    if let Err(error) = crate::ipc::write_response(stream, &response) {
        eprintln!("localityd response failed: {}", error.message());
    }
}

struct DaemonWatchManager {
    config: DaemonConfig,
    file_watcher: NotifyFileWatcher,
    stub_read_watcher: PollingStubReadWatcher,
}

impl DaemonWatchManager {
    fn new(config: &DaemonConfig, runtime: DaemonRuntimeHandle) -> LocalityResult<Self> {
        let file_watcher = NotifyFileWatcher::new({
            let runtime = runtime.clone();
            move |event| {
                if runtime.file_event(event).is_err() {
                    eprintln!("localityd watcher could not submit file event: runtime stopped");
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
                            "localityd stub read watcher could not submit file event: runtime stopped"
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

    fn reload_mounts(&mut self) -> LocalityResult<DaemonReloadReport> {
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

fn should_watch_mount(mount: &MountConfig) -> bool {
    !mount.projection.uses_virtual_filesystem()
}

#[cfg(unix)]
fn remove_stale_socket(socket_path: &Path) -> LocalityResult<()> {
    if !socket_path.exists() {
        return Ok(());
    }

    match UnixStream::connect(socket_path) {
        Ok(_) => Err(LocalityError::InvalidState(format!(
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
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    use locality_core::LocalityError;
    use locality_core::hydration::{HydrationPolicy, HydrationRequest};
    use locality_core::model::MountId;
    use locality_core::pull::PullMode;
    use locality_store::{MountConfig, MountRepository, ProjectionMode, SqliteStateStore};

    use crate::execution::PushJob;
    use crate::hydration::HydrationOutcome;
    use crate::runtime::{RuntimeJobRunner, ScheduledPullRuntimeReport};

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

    #[test]
    fn reload_mounts_primes_virtual_directory_enumeration() {
        let config = test_config("reload-primes-virtual");
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let runtime =
            DaemonRuntime::spawn_with_runner(config.clone(), RecordingRefreshRunner { refresh_tx })
                .expect("spawn runtime");
        let watch_manager = Arc::new(Mutex::new(
            DaemonWatchManager::new(&config, runtime.handle()).expect("watch manager"),
        ));
        let server = DaemonServerHandle {
            runtime: runtime.handle(),
            watch_manager,
            shutdown: Arc::new(AtomicBool::new(false)),
        };

        let mut store = SqliteStateStore::open(config.state_root.clone()).expect("open store");
        store
            .save_mount(
                MountConfig::new(
                    MountId::new("notion-main"),
                    "notion",
                    temp_root("reload-prime-mount"),
                )
                .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save virtual mount");
        drop(store);

        let response = handle_request(DaemonRequest::ReloadMounts, &server);

        assert!(response.ok, "{response:?}");
        let mut refreshes = vec![
            refresh_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("first refresh"),
            refresh_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("second refresh"),
        ];
        refreshes.sort();
        assert_eq!(
            refreshes,
            vec![
                ("notion-main".to_string(), "root".to_string()),
                ("notion-main".to_string(), "source:notion".to_string()),
            ]
        );
        runtime.shutdown();
    }

    struct RecordingRefreshRunner {
        refresh_tx: mpsc::Sender<(String, String)>,
    }

    impl RuntimeJobRunner for RecordingRefreshRunner {
        fn run_pull(&self, _state_root: PathBuf, _path: PathBuf) -> DaemonResponse {
            DaemonResponse::error("unexpected_pull", "pull should not run")
        }

        fn run_push(&self, _state_root: PathBuf, _job: PushJob) -> DaemonResponse {
            DaemonResponse::error("unexpected_push", "push should not run")
        }

        fn run_scheduled_pull(
            &self,
            _state_root: PathBuf,
            _tick: crate::scheduler::PullSchedulerTick,
            _policy: HydrationPolicy,
        ) -> LocalityResult<ScheduledPullRuntimeReport> {
            Err(LocalityError::InvalidState(
                "scheduled pull should not run".to_string(),
            ))
        }

        fn run_hydration(
            &self,
            _state_root: PathBuf,
            _request: HydrationRequest,
        ) -> LocalityResult<HydrationOutcome> {
            Err(LocalityError::InvalidState(
                "hydration should not run".to_string(),
            ))
        }

        fn run_virtual_fs_refresh_children(
            &self,
            _state_root: PathBuf,
            mount_id: String,
            container_identifier: String,
        ) -> LocalityResult<usize> {
            self.refresh_tx
                .send((mount_id, container_identifier))
                .expect("send refresh request");
            Ok(0)
        }
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
            std::env::temp_dir().join(format!("loc-server-{name}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create temp root");
        root
    }
}
