//! Durable state boundary for AgentFS.
//!
//! `afs-store` owns the repository contracts used by the daemon and CLI to load
//! mount configuration, locate projected entities, read last-synced shadows, and
//! journal pushes. The crate provides a deterministic in-memory implementation
//! for tests and a SQLite implementation for local durable state.

pub mod credentials;
pub mod error;
pub mod memory;
pub mod records;
pub mod repository;
pub mod sqlite;

pub use credentials::{
    CredentialError, CredentialResult, CredentialStore, FileCredentialStore,
    InMemoryCredentialStore, open_credential_store,
};
pub use error::{StoreError, StoreResult};
pub use memory::InMemoryStateStore;
pub use records::{
    ConnectionId, ConnectionRecord, ConnectorProfileId, ConnectorProfileRecord, EntityRecord,
    HydrationJobRecord, MountConfig, ProjectionMode, ShadowBlockRecord, ShadowSnapshotRecord,
};
pub use repository::{
    ConnectionRepository, ConnectorProfileRepository, EntityRepository, HydrationJobRepository,
    JournalRepository, MountRepository, ShadowRepository,
};
pub use sqlite::SqliteStateStore;
