pub mod autosave;
pub mod execution;
pub mod file_provider;
pub mod freshness;
pub mod gmail;
pub mod google_docs;
pub mod granola;
pub mod hydration;
pub mod ipc;
pub mod mcp;
pub mod media;
pub mod notion;
pub mod projection_state;
pub mod pull;
pub mod push;
pub mod reconcile;
pub mod runtime;
pub mod scheduler;
pub mod server;
mod shadow_match;
pub mod slack;
pub mod source;
pub mod supervisor;
pub mod virtual_fs;
pub mod virtual_projection;
pub mod watcher;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use locality_core::LocalityResult;
use locality_core::pull::{PullMode, PullSchedulerConfig};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonConfig {
    pub state_root: PathBuf,
    pub tcp_addr: Option<SocketAddr>,
    pub mcp_addr: Option<SocketAddr>,
    pub runtime_tick_interval: Duration,
    pub hydration_retry_delay: Duration,
    pub pull_scheduler: PullSchedulerConfig,
    pub background_connector_sync: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            state_root: default_state_root(),
            tcp_addr: default_tcp_addr(),
            mcp_addr: default_mcp_addr(),
            runtime_tick_interval: Duration::from_secs(1),
            hydration_retry_delay: Duration::from_secs(30),
            pull_scheduler: default_pull_scheduler_config(),
            background_connector_sync: default_background_connector_sync(),
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

    pub fn run_foreground(&self) -> LocalityResult<()> {
        server::run_foreground(&self.config)
    }
}

fn default_state_root() -> PathBuf {
    locality_platform::default_state_root()
}

fn default_tcp_addr() -> Option<SocketAddr> {
    match std::env::var("LOCALITY_DAEMON_TCP_ADDR") {
        Ok(value) if matches!(value.as_str(), "0" | "off" | "none" | "disabled") => None,
        Ok(value) => Some(
            value
                .parse()
                .expect("LOCALITY_DAEMON_TCP_ADDR must be host:port, or off"),
        ),
        Err(_) => Some(crate::ipc::default_tcp_addr()),
    }
}

fn default_mcp_addr() -> Option<SocketAddr> {
    match std::env::var("LOCALITY_MCP_ADDR") {
        Ok(value) if matches!(value.as_str(), "0" | "off" | "none" | "disabled") => None,
        Ok(value) => Some(
            value
                .parse()
                .expect("LOCALITY_MCP_ADDR must be host:port, or off"),
        ),
        Err(_) => Some(crate::mcp::default_mcp_addr()),
    }
}

fn default_pull_scheduler_config() -> PullSchedulerConfig {
    let mut config = PullSchedulerConfig::default();
    if let Ok(value) = std::env::var("LOCALITY_DAEMON_PULL_MODE")
        && matches!(value.as_str(), "relay" | "off" | "disabled")
    {
        config.mode = PullMode::Relay;
    }
    config
}

fn default_background_connector_sync() -> bool {
    !matches!(
        std::env::var("LOCALITY_DAEMON_BACKGROUND_CONNECTOR_SYNC")
            .ok()
            .as_deref(),
        Some("0" | "off" | "none" | "disabled")
    )
}
