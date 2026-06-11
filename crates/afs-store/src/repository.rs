//! Repository traits for durable AgentFS state.
//!
//! The traits are split by lookup responsibility so orchestration code can ask
//! for only the state it needs. `afs diff` and `afs push` primarily need mount
//! config, entity lookup by local path, and the last synced shadow.

use std::path::Path;

use afs_core::journal::{JournalApplyEffect, JournalEntry, JournalStatus, PushId};
use afs_core::model::{MountId, RemoteId};
use afs_core::shadow::ShadowDocument;

use crate::error::StoreResult;
use crate::records::{EntityRecord, MountConfig, ShadowSnapshotRecord};

pub trait MountRepository {
    fn save_mount(&mut self, mount: MountConfig) -> StoreResult<()>;
    fn get_mount(&self, mount_id: &MountId) -> StoreResult<Option<MountConfig>>;
    fn load_mounts(&self) -> StoreResult<Vec<MountConfig>>;
}

pub trait EntityRepository {
    fn save_entity(&mut self, entity: EntityRecord) -> StoreResult<()>;
    fn get_entity(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<EntityRecord>>;
    fn find_entity_by_path(
        &self,
        mount_id: &MountId,
        path: &Path,
    ) -> StoreResult<Option<EntityRecord>>;
    fn list_entities(&self, mount_id: &MountId) -> StoreResult<Vec<EntityRecord>>;
}

pub trait ShadowRepository {
    fn save_shadow(&mut self, mount_id: &MountId, shadow: ShadowDocument) -> StoreResult<()>;
    fn load_shadow(&self, mount_id: &MountId, entity_id: &RemoteId) -> StoreResult<ShadowDocument>;
    fn get_shadow_record(
        &self,
        mount_id: &MountId,
        entity_id: &RemoteId,
    ) -> StoreResult<Option<ShadowSnapshotRecord>>;
}

pub trait JournalRepository {
    fn append_journal(&mut self, entry: JournalEntry) -> StoreResult<()>;
    fn record_journal_apply_effects(
        &mut self,
        push_id: &PushId,
        effects: Vec<JournalApplyEffect>,
    ) -> StoreResult<()>;
    fn update_journal_status(&mut self, push_id: &PushId, status: JournalStatus)
    -> StoreResult<()>;
    fn get_journal(&self, push_id: &PushId) -> StoreResult<Option<JournalEntry>>;
    fn list_journal(&self) -> StoreResult<Vec<JournalEntry>>;

    fn latest_failed_journal_for_entity(
        &self,
        mount_id: &MountId,
        remote_id: &RemoteId,
    ) -> StoreResult<Option<String>> {
        let mut latest = None;
        for journal in self.list_journal()? {
            if journal.mount_id != *mount_id {
                continue;
            }
            if !journal.remote_ids.iter().any(|id| id == remote_id)
                && !journal
                    .plan
                    .affected_entities
                    .iter()
                    .any(|id| id == remote_id)
            {
                continue;
            }
            if let JournalStatus::Failed(message) = journal.status {
                latest = Some((journal.push_id.0, message));
            }
        }

        Ok(latest.map(|(_, message)| message))
    }
}
