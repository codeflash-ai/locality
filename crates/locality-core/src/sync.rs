//! Three-tree sync classification.
//!
//! `plan.md` makes remote/local/synced state the source of truth for direction:
//! local differs from synced means push, remote differs from synced means pull,
//! both differ means conflict unless a later block-aware merge proves the edits
//! disjoint. This module implements only that deterministic classification.

use crate::conflict::BlockChangeSet;
use crate::model::{RemoteId, TreeEntry};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreeTreeSnapshot {
    pub remote: Option<TreeEntry>,
    pub local: Option<TreeEntry>,
    pub synced: Option<TreeEntry>,
}

impl ThreeTreeSnapshot {
    pub fn classify(&self) -> SyncDecision {
        let Some(remote_id) = self.remote_id() else {
            return SyncDecision::Noop;
        };

        let remote_changed = self.remote_changed();
        let local_changed = self.local_changed();

        match (remote_changed, local_changed) {
            (false, false) => SyncDecision::Noop,
            (true, false) if self.remote.is_none() => {
                SyncDecision::DeleteLocalProjection { remote_id }
            }
            (true, false) => SyncDecision::Pull { remote_id },
            (false, true) => SyncDecision::Push { remote_id },
            (true, true) => SyncDecision::Conflict { remote_id },
        }
    }

    pub fn remote_changed(&self) -> bool {
        changed_from_synced(self.remote.as_ref(), self.synced.as_ref())
    }

    pub fn local_changed(&self) -> bool {
        changed_from_synced(self.local.as_ref(), self.synced.as_ref())
    }

    fn remote_id(&self) -> Option<RemoteId> {
        self.remote
            .as_ref()
            .or(self.local.as_ref())
            .or(self.synced.as_ref())
            .map(|entry| entry.remote_id.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncDecision {
    Noop,
    Pull { remote_id: RemoteId },
    Push { remote_id: RemoteId },
    AutoMerge { remote_id: RemoteId },
    Conflict { remote_id: RemoteId },
    DeleteLocalProjection { remote_id: RemoteId },
}

pub fn classify_changes(
    remote_changed: bool,
    local_changed: bool,
    remote_id: RemoteId,
) -> SyncDecision {
    match (remote_changed, local_changed) {
        (false, false) => SyncDecision::Noop,
        (true, false) => SyncDecision::Pull { remote_id },
        (false, true) => SyncDecision::Push { remote_id },
        (true, true) => SyncDecision::Conflict { remote_id },
    }
}

pub fn classify_colliding_edits(
    remote_id: RemoteId,
    remote_changes: &BlockChangeSet,
    local_changes: &BlockChangeSet,
) -> SyncDecision {
    if remote_changes.is_disjoint(local_changes) {
        SyncDecision::AutoMerge { remote_id }
    } else {
        SyncDecision::Conflict { remote_id }
    }
}

fn changed_from_synced(current: Option<&TreeEntry>, synced: Option<&TreeEntry>) -> bool {
    match (current, synced) {
        (None, None) => false,
        (Some(_), None) | (None, Some(_)) => true,
        (Some(current), Some(synced)) => current.differs_from(synced),
    }
}
