pub mod execution;
pub mod hydration;
pub mod notion;
pub mod reconcile;
pub mod scheduler;
pub mod supervisor;
pub mod watcher;

use std::path::PathBuf;

use afs_core::{AfsError, AfsResult};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonConfig {
    pub state_root: PathBuf,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            state_root: PathBuf::from("~/.afs"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Daemon {
    config: DaemonConfig,
}

impl Daemon {
    pub fn new(config: DaemonConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }

    pub fn run_foreground(&self) -> AfsResult<()> {
        Err(AfsError::NotImplemented("daemon supervisor"))
    }
}
