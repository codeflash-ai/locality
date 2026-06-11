use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::thread;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

use afs_core::{AfsError, AfsResult};
use afs_store::{MountConfig, MountRepository, SqliteStateStore};

use crate::DaemonConfig;
use crate::ipc::DaemonResponse;
use crate::runtime::{DaemonRuntime, DaemonRuntimeHandle};
use crate::watcher::{FileWatcher, NotifyFileWatcher, PollingStubReadWatcher};

#[cfg(unix)]
pub fn run_foreground(config: &DaemonConfig) -> AfsResult<()> {
    std::fs::create_dir_all(&config.state_root)?;
    let runtime = DaemonRuntime::spawn(config.clone())?;
    let runtime_handle = runtime.handle();
    let mut file_watcher = NotifyFileWatcher::new({
        let runtime = runtime_handle.clone();
        move |event| {
            if runtime.file_event(event).is_err() {
                eprintln!("afsd watcher could not submit file event: runtime stopped");
            }
        }
    })?;
    let mut stub_read_watcher =
        PollingStubReadWatcher::new(config.state_root.clone(), config.runtime_tick_interval, {
            let runtime = runtime_handle.clone();
            move |event| {
                if runtime.file_event(event).is_err() {
                    eprintln!(
                        "afsd stub read watcher could not submit file event: runtime stopped"
                    );
                }
            }
        })?;
    watch_existing_mounts(config, &mut file_watcher)?;
    watch_existing_mounts(config, &mut stub_read_watcher)?;
    if let Some(addr) = config.tcp_addr {
        let listener = TcpListener::bind(addr).map_err(|error| {
            AfsError::Io(format!("failed to bind daemon TCP listener: {error}"))
        })?;
        let runtime = runtime_handle.clone();
        thread::spawn(move || accept_tcp_connections(listener, runtime));
    }

    let socket_path = crate::ipc::socket_path(&config.state_root);
    remove_stale_socket(&socket_path)?;
    let listener = UnixListener::bind(&socket_path)
        .map_err(|error| AfsError::Io(format!("failed to bind daemon socket: {error}")))?;
    let mounts = load_mounts(config)?;
    print_startup_banner(&socket_path, &mounts);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let runtime = runtime_handle.clone();
                thread::spawn(move || handle_connection(stream, runtime));
            }
            Err(error) => eprintln!("afsd accept failed: {error}"),
        }
    }

    Ok(())
}

#[cfg(unix)]
fn watch_existing_mounts(config: &DaemonConfig, watcher: &mut impl FileWatcher) -> AfsResult<()> {
    for mount in load_mounts(config)? {
        watcher.watch_mount(mount.root)?;
    }

    Ok(())
}

#[cfg(unix)]
fn load_mounts(config: &DaemonConfig) -> AfsResult<Vec<MountConfig>> {
    let store = SqliteStateStore::open(config.state_root.clone()).map_err(AfsError::from)?;
    store.load_mounts().map_err(AfsError::from)
}

#[cfg(unix)]
fn print_startup_banner(socket_path: &Path, mounts: &[MountConfig]) {
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
fn accept_tcp_connections(listener: TcpListener, runtime: DaemonRuntimeHandle) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let runtime = runtime.clone();
                thread::spawn(move || handle_connection(stream, runtime));
            }
            Err(error) => eprintln!("afsd TCP accept failed: {error}"),
        }
    }
}

#[cfg(unix)]
fn handle_connection(mut stream: impl Read + Write, runtime: DaemonRuntimeHandle) {
    let response = match crate::ipc::read_request(&mut stream) {
        Ok(request) => runtime.request(request),
        Err(error) => DaemonResponse::error("bad_request", error.message()),
    };
    write_best_effort(&mut stream, response);
}

#[cfg(unix)]
fn write_best_effort(stream: &mut impl Write, response: DaemonResponse) {
    if let Err(error) = crate::ipc::write_response(stream, &response) {
        eprintln!("afsd response failed: {}", error.message());
    }
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
