use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use afs_cli::daemon::{DaemonRunState, run_daemon_control};

#[test]
fn daemon_status_reports_stopped_when_socket_is_absent() {
    let root = temp_root("afs-cli-daemon-status");

    let args = vec![
        "status".to_string(),
        "--state-dir".to_string(),
        root.display().to_string(),
    ];
    let report = run_daemon_control(&args).expect("daemon status");

    assert!(report.ok);
    assert_eq!(report.state, DaemonRunState::Stopped);
    assert!(report.socket.ends_with("afsd.sock"));

    let _ = fs::remove_dir_all(root);
}

fn temp_root(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{unique}-{suffix}", std::process::id()))
}
