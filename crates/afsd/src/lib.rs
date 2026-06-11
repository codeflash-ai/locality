pub mod execution;
pub mod file_provider;
pub mod hydration;
pub mod ipc;
pub mod notion;
pub mod pull;
pub mod push;
pub mod reconcile;
pub mod runtime;
pub mod scheduler;
pub mod server;
pub mod supervisor;
pub mod watcher;

use std::path::PathBuf;
use std::time::Duration;

use afs_core::AfsResult;
use afs_core::pull::PullSchedulerConfig;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonConfig {
    pub state_root: PathBuf,
    pub runtime_tick_interval: Duration,
    pub hydration_retry_delay: Duration,
    pub pull_scheduler: PullSchedulerConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            state_root: default_state_root(),
            runtime_tick_interval: Duration::from_secs(1),
            hydration_retry_delay: Duration::from_secs(30),
            pull_scheduler: PullSchedulerConfig::default(),
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
        server::run_foreground(&self.config)
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
