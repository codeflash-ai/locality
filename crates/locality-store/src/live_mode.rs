//! Shared Live Mode state-change signaling.
//!
//! Durable Live Mode state lives in the store. Writers that change the
//! source-of-truth state should also publish this local signal so desktop,
//! daemon, or CLI surfaces can wake without watching SQLite WAL churn.

use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{StoreError, StoreResult};
use crate::records::MountLiveModeRecord;
use crate::repository::MountLiveModeRepository;

pub const LIVE_MODE_STATE_CHANGE_SIGNAL_FILE: &str = "live-mode.changed";

#[derive(Debug)]
pub enum MountLiveModeStateChangeError {
    Save(StoreError),
    PublishSignal(StoreError),
}

impl Display for MountLiveModeStateChangeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Save(error) => write!(f, "could not save Live Mode state: {error}"),
            Self::PublishSignal(error) => {
                write!(
                    f,
                    "could not publish Live Mode state-change signal: {error}"
                )
            }
        }
    }
}

impl std::error::Error for MountLiveModeStateChangeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Save(error) | Self::PublishSignal(error) => Some(error),
        }
    }
}

pub fn live_mode_state_change_signal_path(state_root: impl AsRef<Path>) -> PathBuf {
    state_root.as_ref().join(LIVE_MODE_STATE_CHANGE_SIGNAL_FILE)
}

pub fn is_live_mode_state_change_signal_path(
    path: impl AsRef<Path>,
    state_root: impl AsRef<Path>,
) -> bool {
    let Ok(relative) = path.as_ref().strip_prefix(state_root.as_ref()) else {
        return false;
    };
    relative == Path::new(LIVE_MODE_STATE_CHANGE_SIGNAL_FILE)
}

pub fn publish_live_mode_state_change_signal(state_root: impl AsRef<Path>) -> StoreResult<PathBuf> {
    let path = live_mode_state_change_signal_path(state_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, live_mode_state_change_signal_payload())?;
    Ok(path)
}

pub fn save_mount_live_mode_and_publish_signal<R>(
    repository: &mut R,
    state_root: impl AsRef<Path>,
    live_mode: MountLiveModeRecord,
) -> Result<PathBuf, MountLiveModeStateChangeError>
where
    R: MountLiveModeRepository + ?Sized,
{
    repository
        .save_mount_live_mode(live_mode)
        .map_err(MountLiveModeStateChangeError::Save)?;
    publish_live_mode_state_change_signal(state_root)
        .map_err(MountLiveModeStateChangeError::PublishSignal)
}

fn live_mode_state_change_signal_payload() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("unix_ms:{millis}\n")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use locality_core::model::MountId;

    use super::*;
    use crate::memory::InMemoryStateStore;
    use crate::repository::MountLiveModeRepository;

    #[test]
    fn live_mode_signal_path_is_state_root_relative() {
        let temp = TempRoot::new("signal-path");
        let signal_path = live_mode_state_change_signal_path(&temp.state_root);

        assert_eq!(
            signal_path,
            temp.state_root.join(LIVE_MODE_STATE_CHANGE_SIGNAL_FILE)
        );
        assert!(is_live_mode_state_change_signal_path(
            &signal_path,
            &temp.state_root
        ));
        assert!(!is_live_mode_state_change_signal_path(
            temp.state_root.join("state.sqlite3-wal"),
            &temp.state_root
        ));
    }

    #[test]
    fn publish_live_mode_signal_creates_state_root_file() {
        let temp = TempRoot::new("signal-write");

        let signal_path =
            publish_live_mode_state_change_signal(&temp.state_root).expect("publish signal");

        assert_eq!(
            signal_path,
            temp.state_root.join(LIVE_MODE_STATE_CHANGE_SIGNAL_FILE)
        );
        let payload = fs::read_to_string(signal_path).expect("read signal");
        assert!(payload.starts_with("unix_ms:"), "{payload}");
    }

    #[test]
    fn saving_live_mode_state_publishes_signal() {
        let temp = TempRoot::new("signal-save");
        let mut store = InMemoryStateStore::new();
        let record = MountLiveModeRecord::new(MountId::new("notion-main"), true, "unix_ms:1");

        save_mount_live_mode_and_publish_signal(&mut store, &temp.state_root, record.clone())
            .expect("save and signal");

        assert_eq!(
            store
                .get_mount_live_mode(&MountId::new("notion-main"))
                .expect("get live mode"),
            Some(record)
        );
        assert!(
            temp.state_root
                .join(LIVE_MODE_STATE_CHANGE_SIGNAL_FILE)
                .exists()
        );
    }

    struct TempRoot {
        state_root: PathBuf,
    }

    impl TempRoot {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "locality-live-mode-{name}-{}-{unique}-{suffix}",
                std::process::id()
            ));

            Self {
                state_root: root.join("state"),
            }
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            if let Some(root) = self.state_root.parent() {
                let _ = fs::remove_dir_all(root);
            }
        }
    }
}
