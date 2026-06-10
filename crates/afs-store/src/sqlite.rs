//! SQLite state-store placeholder.
//!
//! The SQLite schema should implement the same repository traits as the
//! in-memory store. Keeping this type present lets higher layers depend on the
//! production adapter name while repository behavior is still being designed.

use std::path::{Path, PathBuf};

use afs_core::AfsResult;
use afs_core::journal::{JournalEntry, JournalStatus, JournalStore, PushId};
use afs_core::model::{MountId, RemoteId};
use afs_core::shadow::ShadowDocument;

use crate::error::{StoreError, StoreResult};
use crate::records::{EntityRecord, MountConfig, ShadowSnapshotRecord};
use crate::repository::{EntityRepository, JournalRepository, MountRepository, ShadowRepository};

#[derive(Clone, Debug)]
pub struct SqliteStateStore {
    pub root: PathBuf,
}

impl SqliteStateStore {
    pub fn open(root: PathBuf) -> StoreResult<Self> {
        Ok(Self { root })
    }
}

impl MountRepository for SqliteStateStore {
    fn save_mount(&mut self, _mount: MountConfig) -> StoreResult<()> {
        Err(StoreError::NotImplemented("SQLite mount writes"))
    }

    fn get_mount(&self, _mount_id: &MountId) -> StoreResult<Option<MountConfig>> {
        Err(StoreError::NotImplemented("SQLite mount reads"))
    }

    fn load_mounts(&self) -> StoreResult<Vec<MountConfig>> {
        Err(StoreError::NotImplemented("SQLite mount config loading"))
    }
}

impl EntityRepository for SqliteStateStore {
    fn save_entity(&mut self, _entity: EntityRecord) -> StoreResult<()> {
        Err(StoreError::NotImplemented("SQLite entity writes"))
    }

    fn get_entity(
        &self,
        _mount_id: &MountId,
        _remote_id: &RemoteId,
    ) -> StoreResult<Option<EntityRecord>> {
        Err(StoreError::NotImplemented("SQLite entity reads"))
    }

    fn find_entity_by_path(
        &self,
        _mount_id: &MountId,
        _path: &Path,
    ) -> StoreResult<Option<EntityRecord>> {
        Err(StoreError::NotImplemented("SQLite entity path reads"))
    }

    fn list_entities(&self, _mount_id: &MountId) -> StoreResult<Vec<EntityRecord>> {
        Err(StoreError::NotImplemented("SQLite entity listing"))
    }
}

impl ShadowRepository for SqliteStateStore {
    fn save_shadow(&mut self, _mount_id: &MountId, _shadow: ShadowDocument) -> StoreResult<()> {
        Err(StoreError::NotImplemented("SQLite shadow writes"))
    }

    fn load_shadow(
        &self,
        _mount_id: &MountId,
        _entity_id: &RemoteId,
    ) -> StoreResult<ShadowDocument> {
        Err(StoreError::NotImplemented("SQLite shadow reads"))
    }

    fn get_shadow_record(
        &self,
        _mount_id: &MountId,
        _entity_id: &RemoteId,
    ) -> StoreResult<Option<ShadowSnapshotRecord>> {
        Err(StoreError::NotImplemented("SQLite shadow record reads"))
    }
}

impl JournalRepository for SqliteStateStore {
    fn append_journal(&mut self, _entry: JournalEntry) -> StoreResult<()> {
        Err(StoreError::NotImplemented("SQLite journal append"))
    }

    fn update_journal_status(
        &mut self,
        _push_id: &PushId,
        _status: JournalStatus,
    ) -> StoreResult<()> {
        Err(StoreError::NotImplemented("SQLite journal status update"))
    }

    fn get_journal(&self, _push_id: &PushId) -> StoreResult<Option<JournalEntry>> {
        Err(StoreError::NotImplemented("SQLite journal reads"))
    }

    fn list_journal(&self) -> StoreResult<Vec<JournalEntry>> {
        Err(StoreError::NotImplemented("SQLite journal listing"))
    }
}

impl JournalStore for SqliteStateStore {
    fn append(&mut self, entry: JournalEntry) -> AfsResult<()> {
        self.append_journal(entry).map_err(Into::into)
    }

    fn update_status(&mut self, push_id: &PushId, status: JournalStatus) -> AfsResult<()> {
        self.update_journal_status(push_id, status)
            .map_err(Into::into)
    }
}
