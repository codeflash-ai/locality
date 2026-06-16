//! Hydration policy and request types.
//!
//! Hydration policy stays above the filesystem projection. macOS File Provider
//! mounts materialize online-only items on open, while plain-file mounts can still
//! use marker files as a developer fallback.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::{HydrationState, MountId, RemoteId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydrationPolicy {
    pub auto_hydrate_recent_days: u16,
    pub prefetch_neighbors: bool,
    pub eager_under_page_count: Option<u32>,
}

impl Default for HydrationPolicy {
    fn default() -> Self {
        Self {
            auto_hydrate_recent_days: 90,
            prefetch_neighbors: true,
            eager_under_page_count: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HydrationRequest {
    pub mount_id: MountId,
    pub remote_id: RemoteId,
    pub path: PathBuf,
    pub target_state: HydrationState,
    pub reason: HydrationReason,
}

impl HydrationRequest {
    pub fn new(
        mount_id: MountId,
        remote_id: RemoteId,
        path: impl Into<PathBuf>,
        target_state: HydrationState,
        reason: HydrationReason,
    ) -> Self {
        Self {
            mount_id,
            remote_id,
            path: path.into(),
            target_state,
            reason,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HydrationReason {
    ExplicitPull,
    FileOpen,
    Policy,
    RemoteFastForward,
    StubRead,
    Prefetch,
}

pub fn should_eager_hydrate(workspace_page_count: u32, policy: &HydrationPolicy) -> bool {
    policy
        .eager_under_page_count
        .is_some_and(|threshold| workspace_page_count <= threshold)
}
