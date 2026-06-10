//! Journal contracts for resumable and reversible pushes.
//!
//! The store implementation is responsible for write-ahead durability and fsync.
//! The core keeps the journal entry shape explicit so push orchestration can
//! resume or undo without connector-specific hidden state.

use crate::AfsResult;
use crate::model::{MountId, RemoteId};
use crate::planner::PushPlan;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PushId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub push_id: PushId,
    pub mount_id: MountId,
    pub remote_ids: Vec<RemoteId>,
    pub plan: PushPlan,
    pub status: JournalStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalStatus {
    Prepared,
    Applying,
    Applied,
    Reconciled,
    Reverted,
    Failed(String),
}

pub trait JournalStore {
    fn append(&mut self, entry: JournalEntry) -> AfsResult<()>;
    fn update_status(&mut self, push_id: &PushId, status: JournalStatus) -> AfsResult<()>;
}
