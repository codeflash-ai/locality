use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use afs_core::model::{EntityKind, HydrationState};
use afs_core::{AfsError, AfsResult};
use afs_store::{EntityRepository, MountRepository, SqliteStateStore};
use notify::event::{
    AccessKind, AccessMode, CreateKind, MetadataKind, ModifyKind, RemoveKind, RenameMode,
};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEvent {
    pub path: PathBuf,
    pub kind: FileEventKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileEventKind {
    Read,
    Write,
    Rename,
    Remove,
}

pub trait FileWatcher {
    fn watch_mount(&mut self, root: PathBuf) -> AfsResult<()>;
    fn unwatch_mount(&mut self, _root: &Path) -> AfsResult<()> {
        Ok(())
    }
    fn watched_roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

pub struct NotifyFileWatcher {
    watcher: RecommendedWatcher,
    watched_roots: BTreeSet<PathBuf>,
}

impl NotifyFileWatcher {
    pub fn new(on_event: impl Fn(FileEvent) + Send + 'static) -> AfsResult<Self> {
        let watcher = notify::recommended_watcher(move |event| match event {
            Ok(event) => {
                for file_event in file_events_from_notify_event(event) {
                    on_event(file_event);
                }
            }
            Err(error) => eprintln!("afsd watcher event failed: {error}"),
        })
        .map_err(watcher_error)?;

        Ok(Self {
            watcher,
            watched_roots: BTreeSet::new(),
        })
    }
}

impl FileWatcher for NotifyFileWatcher {
    fn watch_mount(&mut self, root: PathBuf) -> AfsResult<()> {
        if self.watched_roots.contains(&root) {
            return Ok(());
        }
        self.watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(watcher_error)?;
        self.watched_roots.insert(root);
        Ok(())
    }

    fn unwatch_mount(&mut self, root: &Path) -> AfsResult<()> {
        if !self.watched_roots.remove(root) {
            return Ok(());
        }
        self.watcher.unwatch(root).map_err(watcher_error)
    }

    fn watched_roots(&self) -> Vec<PathBuf> {
        self.watched_roots.iter().cloned().collect()
    }
}

pub struct PollingStubReadWatcher {
    watched_roots: Arc<Mutex<BTreeSet<PathBuf>>>,
    stop: Arc<PollStop>,
    join: Option<JoinHandle<()>>,
}

impl PollingStubReadWatcher {
    pub fn new(
        state_root: PathBuf,
        interval: Duration,
        on_event: impl Fn(FileEvent) + Send + 'static,
    ) -> AfsResult<Self> {
        let watched_roots = Arc::new(Mutex::new(BTreeSet::new()));
        let stop = Arc::new(PollStop::default());
        let thread_watched_roots = Arc::clone(&watched_roots);
        let thread_stop = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("afs-stub-read-watcher".to_string())
            .spawn(move || {
                run_stub_access_poll_loop(
                    state_root,
                    interval,
                    thread_watched_roots,
                    thread_stop,
                    on_event,
                );
            })
            .map_err(|error| AfsError::Io(format!("failed to start stub read watcher: {error}")))?;

        Ok(Self {
            watched_roots,
            stop,
            join: Some(join),
        })
    }
}

impl FileWatcher for PollingStubReadWatcher {
    fn watch_mount(&mut self, root: PathBuf) -> AfsResult<()> {
        let mut roots = self.watched_roots.lock().expect("stub read watcher roots");
        roots.insert(root);
        Ok(())
    }

    fn unwatch_mount(&mut self, root: &Path) -> AfsResult<()> {
        let mut roots = self.watched_roots.lock().expect("stub read watcher roots");
        roots.remove(root);
        Ok(())
    }

    fn watched_roots(&self) -> Vec<PathBuf> {
        self.watched_roots
            .lock()
            .expect("stub read watcher roots")
            .iter()
            .cloned()
            .collect()
    }
}

impl Drop for PollingStubReadWatcher {
    fn drop(&mut self) {
        self.stop.stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn file_events_from_notify_event(event: Event) -> Vec<FileEvent> {
    let Some(kind) = file_event_kind(&event.kind) else {
        return Vec::new();
    };

    event
        .paths
        .into_iter()
        .map(|path| FileEvent {
            path,
            kind: kind.clone(),
        })
        .collect()
}

fn file_event_kind(kind: &EventKind) -> Option<FileEventKind> {
    match kind {
        EventKind::Access(access) => access_event_kind(access),
        EventKind::Create(CreateKind::File | CreateKind::Any | CreateKind::Other) => {
            Some(FileEventKind::Write)
        }
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)) => {
            Some(FileEventKind::Read)
        }
        EventKind::Modify(ModifyKind::Data(_))
        | EventKind::Modify(ModifyKind::Metadata(_))
        | EventKind::Modify(ModifyKind::Any)
        | EventKind::Modify(ModifyKind::Other) => Some(FileEventKind::Write),
        EventKind::Modify(ModifyKind::Name(
            RenameMode::Any | RenameMode::Both | RenameMode::From | RenameMode::To,
        )) => Some(FileEventKind::Rename),
        EventKind::Remove(RemoveKind::File | RemoveKind::Any | RemoveKind::Other) => {
            Some(FileEventKind::Remove)
        }
        _ => None,
    }
}

fn access_event_kind(kind: &AccessKind) -> Option<FileEventKind> {
    match kind {
        AccessKind::Open(AccessMode::Write) | AccessKind::Close(AccessMode::Write) => {
            Some(FileEventKind::Write)
        }
        AccessKind::Open(_)
        | AccessKind::Close(AccessMode::Read)
        | AccessKind::Read
        | AccessKind::Any
        | AccessKind::Other => Some(FileEventKind::Read),
        AccessKind::Close(_) => Some(FileEventKind::Read),
    }
}

fn run_stub_access_poll_loop(
    state_root: PathBuf,
    interval: Duration,
    watched_roots: Arc<Mutex<BTreeSet<PathBuf>>>,
    stop: Arc<PollStop>,
    on_event: impl Fn(FileEvent),
) {
    let mut observed = BTreeMap::new();

    loop {
        if let Err(error) =
            poll_stub_accesses(&state_root, &watched_roots, &mut observed, &on_event)
        {
            eprintln!("afsd stub read watcher failed: {error}");
        }

        if stop.wait(interval) {
            break;
        }
    }
}

fn poll_stub_accesses(
    state_root: &std::path::Path,
    watched_roots: &Arc<Mutex<BTreeSet<PathBuf>>>,
    observed: &mut BTreeMap<PathBuf, SystemTime>,
    on_event: &impl Fn(FileEvent),
) -> AfsResult<()> {
    let roots = watched_roots
        .lock()
        .expect("stub read watcher roots")
        .clone();
    if roots.is_empty() {
        return Ok(());
    }

    let store = SqliteStateStore::open(state_root.to_path_buf()).map_err(AfsError::from)?;
    let mut current_stub_paths = BTreeSet::new();

    for mount in store.load_mounts().map_err(AfsError::from)? {
        if mount.projection.uses_virtual_filesystem() {
            continue;
        }
        if !roots.contains(&mount.root) {
            continue;
        }

        for entity in store
            .list_entities(&mount.mount_id)
            .map_err(AfsError::from)?
        {
            if entity.kind != EntityKind::Page {
                continue;
            }

            if !matches!(
                entity.hydration,
                HydrationState::Virtual | HydrationState::Stub
            ) {
                continue;
            }

            let path = mount.root.join(&entity.path);
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(accessed) = metadata.accessed() else {
                continue;
            };

            current_stub_paths.insert(path.clone());
            if let Some(previous) = observed.insert(path.clone(), accessed)
                && accessed > previous
            {
                on_event(FileEvent {
                    path,
                    kind: FileEventKind::Read,
                });
            }
        }
    }

    observed.retain(|path, _| current_stub_paths.contains(path));
    Ok(())
}

#[derive(Default)]
struct PollStop {
    stopped: Mutex<bool>,
    condvar: Condvar,
}

impl PollStop {
    fn wait(&self, duration: Duration) -> bool {
        let stopped = self.stopped.lock().expect("stub read watcher stop lock");
        if *stopped {
            return true;
        }

        let (stopped, _) = self
            .condvar
            .wait_timeout(stopped, duration)
            .expect("stub read watcher stop wait");
        *stopped
    }

    fn stop(&self) {
        let mut stopped = self.stopped.lock().expect("stub read watcher stop lock");
        *stopped = true;
        self.condvar.notify_all();
    }
}

fn watcher_error(error: notify::Error) -> AfsError {
    AfsError::Io(format!("file watcher failed: {error}"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};

    use afs_core::model::{EntityKind, HydrationState, MountId, RemoteId};
    use afs_store::{
        EntityRecord, EntityRepository, MountConfig, MountRepository, ProjectionMode,
        SqliteStateStore,
    };
    use notify::event::{AccessKind, AccessMode, DataChange, MetadataKind, ModifyKind};
    use notify::{Event, EventKind};

    use super::{FileEvent, FileEventKind, file_events_from_notify_event, poll_stub_accesses};

    #[test]
    fn notify_data_modify_maps_to_write_events() {
        let events = file_events_from_notify_event(Event {
            kind: EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            paths: vec![PathBuf::from("Roadmap.md")],
            attrs: Default::default(),
        });

        assert_eq!(
            events,
            vec![FileEvent {
                path: PathBuf::from("Roadmap.md"),
                kind: FileEventKind::Write,
            }]
        );
    }

    #[test]
    fn notify_access_open_maps_to_read_events() {
        let events = file_events_from_notify_event(Event {
            kind: EventKind::Access(AccessKind::Open(AccessMode::Any)),
            paths: vec![PathBuf::from("Roadmap.md")],
            attrs: Default::default(),
        });

        assert_eq!(
            events,
            vec![FileEvent {
                path: PathBuf::from("Roadmap.md"),
                kind: FileEventKind::Read,
            }]
        );
    }

    #[test]
    fn notify_access_time_metadata_maps_to_read_events() {
        let events = file_events_from_notify_event(Event {
            kind: EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)),
            paths: vec![PathBuf::from("Roadmap.md")],
            attrs: Default::default(),
        });

        assert_eq!(
            events,
            vec![FileEvent {
                path: PathBuf::from("Roadmap.md"),
                kind: FileEventKind::Read,
            }]
        );
    }

    #[test]
    fn stub_access_poll_emits_read_when_access_time_advances() {
        let state_root = temp_root("poll-state");
        let mount_root = temp_root("poll-mount");
        let page_path = mount_root.join("Roadmap.md");
        std::fs::write(&page_path, "stub").expect("write stub");

        let mount_id = MountId::new("notion-main");
        let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
        store
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                mount_root.clone(),
            ))
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id,
                    RemoteId::new("page-1"),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save entity");

        let watched_roots = Arc::new(Mutex::new([mount_root].into_iter().collect()));
        let mut observed = [(page_path.clone(), SystemTime::UNIX_EPOCH)]
            .into_iter()
            .collect();
        let events = Arc::new(Mutex::new(Vec::new()));

        poll_stub_accesses(&state_root, &watched_roots, &mut observed, &|event| {
            events.lock().expect("events lock").push(event);
        })
        .expect("poll accesses");

        assert_eq!(
            *events.lock().expect("events lock"),
            vec![FileEvent {
                path: page_path,
                kind: FileEventKind::Read,
            }]
        );
    }

    #[test]
    fn stub_access_poll_ignores_database_directories() {
        let state_root = temp_root("poll-database-state");
        let mount_root = temp_root("poll-database-mount");
        let database_path = mount_root.join("Tasks");
        std::fs::create_dir_all(&database_path).expect("database directory");

        let mount_id = MountId::new("notion-main");
        let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
        store
            .save_mount(MountConfig::new(
                mount_id.clone(),
                "notion",
                mount_root.clone(),
            ))
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id,
                    RemoteId::new("database-1"),
                    EntityKind::Database,
                    "Tasks",
                    "Tasks",
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save entity");

        let watched_roots = Arc::new(Mutex::new([mount_root].into_iter().collect()));
        let mut observed = [(database_path, SystemTime::UNIX_EPOCH)]
            .into_iter()
            .collect();
        let events = Arc::new(Mutex::new(Vec::new()));

        poll_stub_accesses(&state_root, &watched_roots, &mut observed, &|event| {
            events.lock().expect("events lock").push(event);
        })
        .expect("poll accesses");

        assert!(events.lock().expect("events lock").is_empty());
    }

    #[test]
    fn stub_access_poll_skips_virtual_projection_mounts() {
        let state_root = temp_root("poll-virtual-state");
        let mount_root = temp_root("poll-virtual-mount");
        let page_path = mount_root.join("Roadmap.md");
        std::fs::write(&page_path, "stub").expect("write stub");

        let mount_id = MountId::new("notion-fuse");
        let mut store = SqliteStateStore::open(state_root.clone()).expect("open store");
        store
            .save_mount(
                MountConfig::new(mount_id.clone(), "notion", mount_root.clone())
                    .projection(ProjectionMode::LinuxFuse),
            )
            .expect("save mount");
        store
            .save_entity(
                EntityRecord::new(
                    mount_id,
                    RemoteId::new("page-1"),
                    EntityKind::Page,
                    "Roadmap",
                    "Roadmap.md",
                )
                .with_hydration(HydrationState::Stub),
            )
            .expect("save entity");

        let watched_roots = Arc::new(Mutex::new([mount_root].into_iter().collect()));
        let mut observed = [(page_path, SystemTime::UNIX_EPOCH)].into_iter().collect();
        let events = Arc::new(Mutex::new(Vec::new()));

        poll_stub_accesses(&state_root, &watched_roots, &mut observed, &|event| {
            events.lock().expect("events lock").push(event);
        })
        .expect("poll accesses");

        assert!(events.lock().expect("events lock").is_empty());
    }

    fn temp_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("afs-watcher-unit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::thread::sleep(Duration::from_millis(1));
        std::fs::create_dir_all(&root).expect("create temp root");
        root
    }
}
