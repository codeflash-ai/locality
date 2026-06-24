use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use localityd::watcher::{FileEventKind, FileWatcher, NotifyFileWatcher};

static WATCHER_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS FSEvents delivery is not deterministic in headless test runners"
)]
fn notify_watcher_reports_file_writes_under_mount_root() {
    let _guard = watcher_test_guard();
    let root = temp_root("notify-write");
    let (events_tx, events_rx) = mpsc::channel();
    let mut watcher = NotifyFileWatcher::new(move |event| {
        let _ = events_tx.send(event);
    })
    .expect("create watcher");
    watcher.watch_mount(root.clone()).expect("watch mount");
    std::thread::sleep(Duration::from_millis(500));

    let path = root.join("Roadmap.md");
    let canonical_root = std::fs::canonicalize(&root).ok();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_write = false;
    let mut observed = Vec::new();

    for attempt in 0..40 {
        std::fs::write(&path, format!("edited {attempt}")).expect("write file");
        let wait_until = Instant::now() + Duration::from_millis(250);
        while Instant::now() < wait_until {
            let Ok(event) = events_rx.recv_timeout(Duration::from_millis(50)) else {
                continue;
            };
            observed.push(event.clone());
            if event_is_under_root(&event.path, &root, canonical_root.as_ref())
                && event.kind == FileEventKind::Write
            {
                saw_write = true;
                break;
            }
        }
        if saw_write || Instant::now() >= deadline {
            break;
        }
    }

    assert!(
        saw_write,
        "watcher did not report write for {:?}; observed {:?}",
        path, observed
    );
}

fn event_is_under_root(
    event_path: &PathBuf,
    root: &PathBuf,
    canonical_root: Option<&PathBuf>,
) -> bool {
    event_path == root
        || event_path.starts_with(root)
        || canonical_root.is_some_and(|root| event_path == root || event_path.starts_with(root))
}

#[test]
fn notify_watcher_tracks_and_unwatches_roots() {
    let _guard = watcher_test_guard();
    let root = temp_root("notify-unwatch");
    let mut watcher = NotifyFileWatcher::new(move |_| {}).expect("create watcher");

    watcher.watch_mount(root.clone()).expect("watch mount");
    watcher
        .watch_mount(root.clone())
        .expect("watch mount again");
    assert_eq!(watcher.watched_roots(), vec![root.clone()]);

    watcher.unwatch_mount(&root).expect("unwatch mount");
    assert!(watcher.watched_roots().is_empty());
}

fn watcher_test_guard() -> MutexGuard<'static, ()> {
    WATCHER_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn temp_root(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "loc-watcher-{name}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}
